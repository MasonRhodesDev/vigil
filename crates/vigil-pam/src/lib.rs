//! PAM authentication worker for vigil-lock (DESIGN.md §12).
//!
//! One worker thread per attempt: `pam_authenticate` blocks inside the
//! conversation until the UI supplies a response, so the conversation runs
//! off the UI thread and reports through [`vigil_core::AuthEvent`]s via a
//! caller-supplied emit closure (the binary drains them on its tick).
//! Cancellation drops the response channel: the blocked conversation errors
//! out and the PAM transaction unwinds cleanly.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use vigil_core::{AuthError, AuthEvent};

/// The PAM service to use: our own file when packaged, else the login stack
/// (verbatim what hyprlock/swaylock ship: `auth include login`).
pub fn service_name() -> &'static str {
    if Path::new("/etc/pam.d/vigil-lock").exists() {
        "vigil-lock"
    } else {
        "login"
    }
}

struct Bridge<E: Fn(AuthEvent)> {
    emit: E,
    responses: mpsc::Receiver<String>,
    /// Set the moment `ask` cannot answer PAM.
    ///
    /// The return code alone cannot tell us this happened: pam_unix maps a
    /// failed conversation onto `PAM_AUTH_ERR` (journal: "conversation
    /// failed", then "auth could not identify password"), the very same code
    /// it returns for a wrong password. The only side that knows the
    /// difference is this one, so it records it (issue #91).
    broken: Arc<AtomicBool>,
}

impl<E: Fn(AuthEvent)> Bridge<E> {
    fn new(emit: E, responses: mpsc::Receiver<String>) -> Self {
        Self {
            emit,
            responses,
            broken: Arc::default(),
        }
    }

    fn ask(&mut self, text: &CStr, secret: bool) -> Result<CString, ErrorCode> {
        (self.emit)(AuthEvent::Prompt {
            text: text.to_string_lossy().into_owned(),
            secret,
        });
        let broken = self.broken.clone();
        let fail = move || {
            broken.store(true, Ordering::SeqCst);
            ErrorCode::CONV_ERR
        };
        let response = self.responses.recv().map_err(|_| fail())?;
        CString::new(response).map_err(|_| fail())
    }
}

impl<E: Fn(AuthEvent)> ConversationHandler for Bridge<E> {
    fn prompt_echo_on(&mut self, msg: &CStr) -> Result<CString, ErrorCode> {
        self.ask(msg, false)
    }
    fn prompt_echo_off(&mut self, msg: &CStr) -> Result<CString, ErrorCode> {
        self.ask(msg, true)
    }
    fn text_info(&mut self, msg: &CStr) {
        (self.emit)(AuthEvent::Info(msg.to_string_lossy().into_owned()));
    }
    fn error_msg(&mut self, msg: &CStr) {
        (self.emit)(AuthEvent::Error(msg.to_string_lossy().into_owned()));
    }
}

/// One in-flight authentication attempt.
pub struct PamAttempt {
    responses: Option<mpsc::Sender<String>>,
    worker: Option<JoinHandle<()>>,
}

impl PamAttempt {
    /// Spawn the worker and begin the conversation for `user`. `emit` is
    /// called from the worker thread for every [`AuthEvent`], ending with
    /// exactly one `Done`.
    pub fn start(user: &str, emit: impl Fn(AuthEvent) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let user = user.to_owned();
        let worker = std::thread::spawn(move || {
            let bridge = Bridge::new(&emit, rx);
            let result = authenticate(&user, bridge);
            emit(AuthEvent::Done(result));
        });
        Self {
            responses: Some(tx),
            worker: Some(worker),
        }
    }

    /// Feed the answer to the outstanding prompt.
    pub fn respond(&self, text: String) {
        if let Some(tx) = &self.responses {
            let _ = tx.send(text);
        }
    }

    /// Abort the attempt: the conversation's recv fails and PAM unwinds.
    /// The worker still emits its `Done(Err)` before exiting.
    pub fn cancel(&mut self) {
        self.responses = None;
    }

    /// Abandon the worker thread: cancel the conversation and let the thread
    /// finish (or not) on its own. A PAM module can block outside the
    /// conversation entirely — pam_fprintd waits on a D-Bus verify that only
    /// ends when a finger arrives — so there is no bound under which joining
    /// is safe. Unlock and process exit must never wait on PAM.
    pub fn detach(&mut self) {
        self.responses = None;
        drop(self.worker.take());
    }
}

impl Drop for PamAttempt {
    fn drop(&mut self) {
        // Deliberately no join: a worker wedged inside a PAM module would
        // wedge this drop — and this drop runs on the unlock path (issue
        // #49: lockers that survived unlock and logout were stuck here).
        self.detach();
    }
}

/// Sort a PAM failure into "PAM judged the credential" and "the
/// conversation broke before it could" (issue #91).
///
/// `conversation_broke` wins over the code, because it is the only reliable
/// signal: a module that never got its answer reports `PAM_AUTH_ERR` like
/// any wrong password. The codes listed here are the ones that mean the
/// exchange itself failed even when our own bridge stayed healthy; every
/// other code is PAM's verdict on the user (`AUTH_ERR`, `USER_UNKNOWN`,
/// `MAXTRIES` from a faillock lockout, an expired credential) or a module
/// fault the user must be told about rather than have retried behind their
/// back.
fn classify(code: ErrorCode, message: String, conversation_broke: bool) -> AuthError {
    let transport = matches!(
        code,
        ErrorCode::CONV_ERR
            | ErrorCode::CONV_AGAIN
            | ErrorCode::ABORT
            | ErrorCode::BUF_ERR
            | ErrorCode::INCOMPLETE
    );
    if conversation_broke || transport {
        AuthError::Conversation(message)
    } else {
        AuthError::Denied(message)
    }
}

fn authenticate<E: Fn(AuthEvent)>(user: &str, bridge: Bridge<&E>) -> Result<(), AuthError> {
    // Cloned out before the bridge is moved into the context, which owns it
    // for the rest of the transaction.
    let broken = bridge.broken.clone();
    let mut ctx = Context::new(service_name(), Some(user), bridge)
        .map_err(|e| AuthError::Conversation(format!("pam context: {e}")))?;
    // Authentication ONLY — deliberately no `acct_mgmt`, matching hyprlock/
    // swaylock. The user is already logged in (account validity was settled
    // at login), and pam_unix's account phase needs the setuid unix_chkpwd
    // helper, which fails from a systemd user-service context (hypridle →
    // vigil-lock): the correct password would be REJECTED at unlock.
    ctx.authenticate(Flag::NONE)
        .map_err(|e| classify(e.code(), e.to_string(), broken.load(Ordering::SeqCst)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The channel bridge is testable without libpam: drive the
    /// Conversation impl directly.
    #[test]
    fn bridge_round_trips_prompt_and_response() {
        let events: Arc<Mutex<Vec<AuthEvent>>> = Arc::default();
        let (tx, rx) = mpsc::channel();
        let sink = events.clone();
        let mut bridge = Bridge::new(move |e| sink.lock().unwrap().push(e), rx);
        tx.send("hunter2".into()).unwrap();
        let out = bridge
            .prompt_echo_off(&CString::new("Password:").unwrap())
            .unwrap();
        assert_eq!(out.to_str().unwrap(), "hunter2");
        assert_eq!(
            events.lock().unwrap()[0],
            AuthEvent::Prompt {
                text: "Password:".into(),
                secret: true
            }
        );
    }

    #[test]
    fn dropped_response_channel_aborts_conversation() {
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx);
        let mut bridge = Bridge::new(|_| {}, rx);
        let err = bridge
            .prompt_echo_off(&CString::new("Password:").unwrap())
            .unwrap_err();
        assert_eq!(err, ErrorCode::CONV_ERR);
    }

    /// Issue #91. A dropped sender and a wrong password must not land in the
    /// same bucket: the first is vigil breaking its own conversation, the
    /// second is PAM's verdict. pam_unix reports BOTH as `PAM_AUTH_ERR`, so
    /// the bridge's own record of the break is what has to decide.
    #[test]
    fn a_broken_conversation_is_not_a_denial() {
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx);
        let mut bridge = Bridge::new(|_| {}, rx);
        assert!(!bridge.broken.load(Ordering::SeqCst));
        let code = bridge
            .prompt_echo_off(&CString::new("Password:").unwrap())
            .unwrap_err();
        assert!(
            bridge.broken.load(Ordering::SeqCst),
            "the bridge must record that it could not answer PAM"
        );

        // What pam_unix actually returns after a failed conversation.
        let outcome = classify(
            ErrorCode::AUTH_ERR,
            "Authentication failure".into(),
            bridge.broken.load(Ordering::SeqCst),
        );
        assert_eq!(
            outcome,
            AuthError::Conversation("Authentication failure".into()),
            "a conversation vigil broke was reported as a credential denial"
        );
        assert_eq!(code, ErrorCode::CONV_ERR);
    }

    #[test]
    fn a_rejected_credential_is_a_denial() {
        for code in [
            ErrorCode::AUTH_ERR,
            ErrorCode::USER_UNKNOWN,
            ErrorCode::PERM_DENIED,
            // A faillock lockout IS a verdict on the user: they must see it.
            ErrorCode::MAXTRIES,
        ] {
            assert_eq!(
                classify(code, "no".into(), false),
                AuthError::Denied("no".into()),
                "{code:?}"
            );
        }
    }

    #[test]
    fn transport_codes_are_conversation_failures_even_with_a_healthy_bridge() {
        for code in [
            ErrorCode::CONV_ERR,
            ErrorCode::CONV_AGAIN,
            ErrorCode::ABORT,
            ErrorCode::BUF_ERR,
            ErrorCode::INCOMPLETE,
        ] {
            assert!(
                classify(code, "no".into(), false).is_conversation(),
                "{code:?}"
            );
        }
    }

    #[test]
    fn drop_never_waits_on_a_wedged_worker() {
        // Issue #49: a PAM module can block outside the conversation
        // (pam_fprintd waiting on a finger). Dropping the attempt — which
        // happens on every unlock/teardown path — must return immediately,
        // not join the thread.
        let (tx, _rx) = mpsc::channel();
        let attempt = PamAttempt {
            responses: Some(tx),
            worker: Some(std::thread::spawn(|| {
                loop {
                    std::thread::park();
                }
            })),
        };
        let start = std::time::Instant::now();
        drop(attempt);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "drop blocked for {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn info_and_error_pass_through() {
        let events: Arc<Mutex<Vec<AuthEvent>>> = Arc::default();
        let (_tx, rx) = mpsc::channel();
        let sink = events.clone();
        let mut bridge = Bridge::new(move |e| sink.lock().unwrap().push(e), rx);
        bridge.text_info(&CString::new("fp: place finger").unwrap());
        bridge.error_msg(&CString::new("bad").unwrap());
        let got = events.lock().unwrap();
        assert_eq!(got[0], AuthEvent::Info("fp: place finger".into()));
        assert_eq!(got[1], AuthEvent::Error("bad".into()));
    }
}
