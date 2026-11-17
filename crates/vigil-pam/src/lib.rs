use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use vigil_core::{AuthError, AuthEvent};

pub mod faillock;

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

pub struct PamAttempt {
    responses: Option<mpsc::Sender<String>>,
    worker: Option<JoinHandle<()>>,
}

impl PamAttempt {
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

    pub fn respond(&self, text: String) {
        if let Some(tx) = &self.responses {
            let _ = tx.send(text);
        }
    }

    pub fn cancel(&mut self) {
        self.responses = None;
    }

    pub fn detach(&mut self) {
        self.responses = None;
        drop(self.worker.take());
    }
}

impl Drop for PamAttempt {
    fn drop(&mut self) {
        self.detach();
    }
}

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
    let broken = bridge.broken.clone();
    let mut ctx = Context::new(service_name(), Some(user), bridge)
        .map_err(|e| AuthError::Conversation(format!("pam context: {e}")))?;
    ctx.authenticate(Flag::NONE)
        .map_err(|e| classify(e.code(), e.to_string(), broken.load(Ordering::SeqCst)))
        .map_err(|failure| lockout_aware(user, failure))?;
    Ok(())
}

fn lockout_aware(user: &str, failure: AuthError) -> AuthError {
    let AuthError::Denied(message) = failure else {
        return failure;
    };
    match faillock::read(user) {
        Some(faillock::Lockout::Until(remaining)) => AuthError::Locked {
            message,
            remaining: Some(remaining),
        },
        Some(faillock::Lockout::Indefinite) => AuthError::Locked {
            message,
            remaining: None,
        },
        None => AuthError::Denied(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
