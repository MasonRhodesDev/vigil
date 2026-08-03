//! Auth state machine (DESIGN.md §5): the greetd IPC conversation
//! (create_session -> auth_message* -> post_auth_message_response ->
//! start_session), rendering PAM messages verbatim through
//! [`vigil_core::AuthUi`] and consuming [`vigil_core::UiMessage`]s.
//! Session listing from /usr/share/{wayland-,x}sessions lands in M2.

use vigil_core::{AuthUi, UiMessage};

#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "auth: {}", self.0)
    }
}
impl std::error::Error for AuthError {}

/// One greetd conversation. Connects to `$GREETD_SOCK` (or an injected
/// socket path — the fake-greetd tests use this).
pub struct AuthMachine {
    _private: (),
}

impl AuthMachine {
    pub fn connect(_socket: Option<&str>) -> Result<Self, AuthError> {
        todo!("M1: greetd_ipc sync codec over $GREETD_SOCK")
    }

    /// Begin (or restart) the conversation for `_user`.
    pub fn start(&mut self, _user: &str, _ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        todo!("M1")
    }

    /// Feed a UI message into the conversation; on success this eventually
    /// calls start_session and the process exits.
    pub fn handle(&mut self, _msg: UiMessage, _ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        todo!("M1")
    }
}
