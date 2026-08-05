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
use std::sync::mpsc;
use std::thread::JoinHandle;

use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use vigil_core::AuthEvent;

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
}

impl<E: Fn(AuthEvent)> Bridge<E> {
    fn ask(&mut self, text: &CStr, secret: bool) -> Result<CString, ErrorCode> {
        (self.emit)(AuthEvent::Prompt {
            text: text.to_string_lossy().into_owned(),
            secret,
        });
        let response = self.responses.recv().map_err(|_| ErrorCode::CONV_ERR)?;
        CString::new(response).map_err(|_| ErrorCode::CONV_ERR)
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
            let bridge = Bridge {
                emit: &emit,
                responses: rx,
            };
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
}

impl Drop for PamAttempt {
    fn drop(&mut self) {
        self.responses = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn authenticate<E: Fn(AuthEvent)>(user: &str, bridge: Bridge<&E>) -> Result<(), String> {
    let mut ctx = Context::new(service_name(), Some(user), bridge)
        .map_err(|e| format!("pam context: {e}"))?;
    // Authentication ONLY — deliberately no `acct_mgmt`, matching hyprlock/
    // swaylock. The user is already logged in (account validity was settled
    // at login), and pam_unix's account phase needs the setuid unix_chkpwd
    // helper, which fails from a systemd user-service context (hypridle →
    // vigil-lock): the correct password would be REJECTED at unlock.
    ctx.authenticate(Flag::NONE).map_err(|e| e.to_string())?;
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
        let mut bridge = Bridge {
            emit: move |e| sink.lock().unwrap().push(e),
            responses: rx,
        };
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
        let mut bridge = Bridge {
            emit: |_| {},
            responses: rx,
        };
        let err = bridge
            .prompt_echo_off(&CString::new("Password:").unwrap())
            .unwrap_err();
        assert_eq!(err, ErrorCode::CONV_ERR);
    }

    #[test]
    fn info_and_error_pass_through() {
        let events: Arc<Mutex<Vec<AuthEvent>>> = Arc::default();
        let (_tx, rx) = mpsc::channel();
        let sink = events.clone();
        let mut bridge = Bridge {
            emit: move |e| sink.lock().unwrap().push(e),
            responses: rx,
        };
        bridge.text_info(&CString::new("fp: place finger").unwrap());
        bridge.error_msg(&CString::new("bad").unwrap());
        let got = events.lock().unwrap();
        assert_eq!(got[0], AuthEvent::Info("fp: place finger".into()));
        assert_eq!(got[1], AuthEvent::Error("bad".into()));
    }
}
