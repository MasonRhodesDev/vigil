//! Auth state machine (DESIGN.md §4): the greetd IPC conversation
//! (create_session -> auth_message* -> post_auth_message_response ->
//! start_session), rendering PAM messages verbatim through
//! [`vigil_core::AuthUi`] and consuming [`vigil_core::UiMessage`]s.
//! Session listing from /usr/share/{wayland-,x}sessions lands in M2.
//!
//! The synchronous greetd codec is used here. Each request/response round trip
//! can therefore block briefly; a calloop-based owner should call this API from
//! event-loop callbacks only when that small pause is acceptable.

use std::env;
use std::os::unix::net::UnixStream;

use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{AuthMessageType, Request, Response};
use vigil_core::{AuthUi, UiMessage};

#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "auth: {}", self.0)
    }
}
impl std::error::Error for AuthError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// The greeter itself is asking who is logging in; greetd has not been
    /// contacted yet. Entered only when no fixed user was configured.
    AwaitingUser,
    AwaitingPrompt,
    Complete,
}

/// Prompt text for the greeter-owned username stage.
pub const USERNAME_PROMPT: &str = "Username";

/// One greetd conversation. Connects to the explicitly supplied socket, or
/// falls back to `$GREETD_SOCK` when no path is supplied.
pub struct AuthMachine {
    stream: UnixStream,
    command: Vec<String>,
    env: Vec<String>,
    state: State,
    user: Option<String>,
    default_user: Option<String>,
    /// Whether the user was fixed at startup (kiosk/autologin style). When
    /// false, the machine owns a username stage and Cancel returns to it.
    fixed_user: bool,
}

impl AuthMachine {
    pub fn connect(socket: Option<&str>) -> Result<Self, AuthError> {
        let path = match socket {
            Some(path) => path.to_owned(),
            None => env::var("GREETD_SOCK")
                .map_err(|error| AuthError(format!("GREETD_SOCK is not set: {error}")))?,
        };
        let stream = UnixStream::connect(&path)
            .map_err(|error| AuthError(format!("connect to {path}: {error}")))?;
        Ok(Self {
            stream,
            command: Vec::new(),
            env: Vec::new(),
            state: State::Idle,
            user: None,
            default_user: None,
            fixed_user: false,
        })
    }

    /// Set the command and environment passed verbatim to greetd when
    /// authentication succeeds.
    pub fn set_session(&mut self, command: Vec<String>, env: Vec<String>) {
        self.command = command;
        self.env = env;
    }

    /// Username submitted with an empty field falls back to this (the
    /// remembered last user); with none set, empty re-prompts.
    pub fn set_default_user(&mut self, user: Option<String>) {
        self.default_user = user;
    }

    /// The user of the current conversation, once known.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Whether greetd accepted `start_session` and the greeter may finish.
    pub fn is_complete(&self) -> bool {
        self.state == State::Complete
    }

    /// Begin the greeter conversation. With a fixed `user` (kiosk/autologin
    /// style) this goes straight to greetd; without one the greeter asks for
    /// the username first — greetd is not contacted until it is submitted.
    pub fn begin(&mut self, user: Option<&str>, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        match user {
            Some(user) => {
                self.fixed_user = true;
                self.start(user, ui)
            }
            None => {
                self.fixed_user = false;
                self.prompt_username(ui);
                Ok(())
            }
        }
    }

    /// Begin or switch an account selected by the greeter's user list.
    ///
    /// Unlike [`Self::begin`] with `Some`, a listed account is not fixed:
    /// the user may switch accounts or choose manual entry later. A selected
    /// account goes directly to greetd's first authentication prompt; `None`
    /// is the explicit manual-entry choice and shows the username prompt.
    pub fn select_user(
        &mut self,
        user: Option<&str>,
        ui: &mut dyn AuthUi,
    ) -> Result<(), AuthError> {
        if self.state == State::Complete {
            return Err(AuthError("session has already started".into()));
        }
        self.fixed_user = false;
        if self.state == State::AwaitingPrompt {
            self.cancel(ui)?;
        }
        match user {
            Some(user) => self.start(user, ui),
            None => {
                self.user = None;
                self.prompt_username(ui);
                Ok(())
            }
        }
    }

    fn prompt_username(&mut self, ui: &mut dyn AuthUi) {
        ui.show_prompt(USERNAME_PROMPT, false);
        self.state = State::AwaitingUser;
    }

    /// Begin a conversation for `user`. After a failure or cancellation the
    /// same machine can be started again without reconnecting.
    pub fn start(&mut self, user: &str, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        if self.state == State::Complete {
            return Err(AuthError("session has already started".into()));
        }
        if self.state == State::AwaitingPrompt {
            self.cancel(ui)?;
        }
        self.user = Some(user.to_owned());

        let response = self.transact(
            &Request::CreateSession {
                username: user.to_owned(),
            },
            ui,
        )?;
        self.process_response(response, ui)
    }

    /// Feed a UI message into the conversation. A response is valid only while
    /// a visible or secret prompt is outstanding.
    pub fn handle(&mut self, msg: UiMessage, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        match msg {
            UiMessage::Respond(username) if self.state == State::AwaitingUser => {
                let username = username.trim();
                if username.is_empty() {
                    if let Some(default) = self.default_user.clone() {
                        return self.start(&default, ui);
                    }
                    self.prompt_username(ui);
                    return Ok(());
                }
                self.start(username, ui)
            }
            UiMessage::Respond(response) if self.state == State::AwaitingPrompt => {
                let response = self.transact(
                    &Request::PostAuthMessageResponse {
                        response: Some(response),
                    },
                    ui,
                )?;
                self.process_response(response, ui)
            }
            UiMessage::Cancel if self.state == State::AwaitingPrompt => {
                self.cancel(ui)?;
                if !self.fixed_user {
                    self.prompt_username(ui);
                }
                Ok(())
            }
            UiMessage::Respond(_) => Err(AuthError("no authentication prompt is pending".into())),
            UiMessage::Cancel => {
                if self.state == State::Idle && !self.fixed_user {
                    self.prompt_username(ui);
                }
                Ok(())
            }
            // Session choice is the binary's product logic (it owns the
            // enumerated list); it never reaches greetd through here.
            UiMessage::SelectSession(_) | UiMessage::SelectUser(_) => Ok(()),
            UiMessage::Power(_) => Err(AuthError("power actions are not authentication".into())),
        }
    }

    fn cancel(&mut self, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        let response = self.transact(&Request::CancelSession, ui)?;
        self.state = State::Idle;
        match response {
            Response::Success => Ok(()),
            Response::Error { description, .. } => Err(AuthError(description)),
            Response::AuthMessage { .. } => Err(AuthError(
                "greetd returned an authentication message after cancellation".into(),
            )),
        }
    }

    fn process_response(
        &mut self,
        mut response: Response,
        ui: &mut dyn AuthUi,
    ) -> Result<(), AuthError> {
        // An auth failure restarts the conversation, and the fresh prompt
        // clears UI messages per the contract — so the failure is re-raised
        // AFTER that prompt or the user never sees why they are retyping.
        let mut pending_error: Option<String> = None;
        loop {
            match response {
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Visible,
                    auth_message,
                } => {
                    ui.show_prompt(&auth_message, false);
                    if let Some(error) = pending_error.take() {
                        ui.show_error(&error);
                    }
                    self.state = State::AwaitingPrompt;
                    return Ok(());
                }
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message,
                } => {
                    ui.show_prompt(&auth_message, true);
                    if let Some(error) = pending_error.take() {
                        ui.show_error(&error);
                    }
                    self.state = State::AwaitingPrompt;
                    return Ok(());
                }
                Response::AuthMessage {
                    auth_message_type,
                    auth_message,
                } => {
                    match auth_message_type {
                        AuthMessageType::Info => ui.show_info(&auth_message),
                        AuthMessageType::Error => ui.show_error(&auth_message),
                        AuthMessageType::Visible | AuthMessageType::Secret => unreachable!(),
                    }
                    response =
                        self.transact(&Request::PostAuthMessageResponse { response: None }, ui)?;
                }
                Response::Success => {
                    let request = Request::StartSession {
                        cmd: self.command.clone(),
                        env: self.env.clone(),
                    };
                    match self.transact(&request, ui)? {
                        Response::Success => {
                            self.state = State::Complete;
                            return Ok(());
                        }
                        Response::Error { description, .. } => {
                            ui.show_error(&description);
                            self.state = State::Idle;
                            return Ok(());
                        }
                        Response::AuthMessage { .. } => {
                            return Err(AuthError(
                                "greetd returned an authentication message after start_session"
                                    .into(),
                            ));
                        }
                    }
                }
                Response::Error { description, .. } => {
                    ui.show_error(&description);
                    self.state = State::Idle;
                    let Some(user) = self.user.clone() else {
                        return Ok(());
                    };
                    // Best-effort session teardown, then a fresh conversation
                    // so the next submission has a prompt to answer. A create
                    // failure here is surfaced, not looped on.
                    let _ = self.transact(&Request::CancelSession, ui);
                    pending_error = Some(description);
                    response = self.transact(&Request::CreateSession { username: user }, ui)?;
                }
            }
        }
    }

    fn transact(&mut self, request: &Request, ui: &mut dyn AuthUi) -> Result<Response, AuthError> {
        ui.set_busy(true);
        let result = request
            .write_to(&mut self.stream)
            .and_then(|()| Response::read_from(&mut self.stream))
            .map_err(|error| AuthError(format!("greetd IPC: {error}")));
        ui.set_busy(false);
        result
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    use greetd_ipc::ErrorType;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum UiCall {
        Prompt(String, bool),
        Info(String),
        Error(String),
        Busy(bool),
    }

    #[derive(Default)]
    struct RecordingUi(Vec<UiCall>);

    impl AuthUi for RecordingUi {
        fn show_prompt(&mut self, text: &str, secret: bool) {
            self.0.push(UiCall::Prompt(text.into(), secret));
        }
        fn show_info(&mut self, text: &str) {
            self.0.push(UiCall::Info(text.into()));
        }
        fn show_error(&mut self, text: &str) {
            self.0.push(UiCall::Error(text.into()));
        }
        fn set_busy(&mut self, busy: bool) {
            self.0.push(UiCall::Busy(busy));
        }
    }

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

    struct FakeServer {
        path: PathBuf,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FakeServer {
        fn spawn(script: impl FnOnce(&mut UnixStream) + Send + 'static) -> Option<Self> {
            let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
            let dir = env::temp_dir().join(format!("vigil-auth-{}-{id}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let probe_path = dir.join("probe.sock");
            let probe = (|| -> std::io::Result<()> {
                let listener = UnixListener::bind(&probe_path)?;
                let mut client = UnixStream::connect(&probe_path)?;
                client.write_all(&[0])?;
                let (mut server, _) = listener.accept()?;
                let mut byte = [0];
                server.read_exact(&mut byte)
            })();
            if let Err(error) = probe {
                fs::remove_dir_all(&dir).unwrap();
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    return None;
                }
                panic!("probe fake greetd socket: {error}");
            }
            fs::remove_file(&probe_path).unwrap();
            let path = dir.join("greetd.sock");
            let listener = match UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    // Some sandboxes prohibit AF_UNIX even under writable roots.
                    fs::remove_dir_all(&dir).unwrap();
                    return None;
                }
                Err(error) => panic!("bind fake greetd socket: {error}"),
            };
            let thread = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                script(&mut stream);
            });
            Some(Self {
                path,
                thread: Some(thread),
            })
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            fs::remove_dir_all(self.path.parent().unwrap()).unwrap();
        }
    }

    fn request(stream: &mut UnixStream) -> Request {
        Request::read_from(stream).unwrap()
    }

    fn respond(stream: &mut UnixStream, response: Response) {
        response.write_to(stream).unwrap();
    }

    fn assert_create(request: Request, username: &str) {
        match request {
            Request::CreateSession { username: actual } => assert_eq!(actual, username),
            other => panic!("expected create_session, got {other:?}"),
        }
    }

    fn assert_answer(request: Request, expected: Option<&str>) {
        match request {
            Request::PostAuthMessageResponse { response } => {
                assert_eq!(response.as_deref(), expected);
            }
            other => panic!("expected auth response, got {other:?}"),
        }
    }

    fn assert_cancel(request: Request) {
        assert!(
            matches!(request, Request::CancelSession),
            "expected cancel_session, got {request:?}"
        );
    }

    fn assert_start(request: Request) {
        match request {
            Request::StartSession { cmd, env } => {
                assert_eq!(cmd, ["sway", "--unsupported-gpu"]);
                assert_eq!(env, ["XDG_SESSION_TYPE=wayland"]);
            }
            other => panic!("expected start_session, got {other:?}"),
        }
    }

    fn machine(server: &FakeServer) -> AuthMachine {
        let mut machine = AuthMachine::connect(Some(server.path.to_str().unwrap())).unwrap();
        machine.set_session(
            vec!["sway".into(), "--unsupported-gpu".into()],
            vec!["XDG_SESSION_TYPE=wayland".into()],
        );
        machine
    }

    fn auth_message(kind: AuthMessageType, text: &str) -> Response {
        Response::AuthMessage {
            auth_message_type: kind,
            auth_message: text.into(),
        }
    }

    #[test]
    fn succeeds_after_one_secret_prompt() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_answer(request(stream), Some("hunter2"));
            respond(stream, Response::Success);
            assert_start(request(stream));
            respond(stream, Response::Success);
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.start("alice", &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("hunter2".into()), &mut ui)
            .unwrap();
        assert!(machine.is_complete());
        assert_eq!(
            ui.0,
            [
                UiCall::Busy(true),
                UiCall::Busy(false),
                UiCall::Prompt("Password:".into(), true),
                UiCall::Busy(true),
                UiCall::Busy(false),
                UiCall::Busy(true),
                UiCall::Busy(false),
            ]
        );
    }

    #[test]
    fn auth_error_then_retry() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_answer(request(stream), Some("wrong"));
            respond(
                stream,
                Response::Error {
                    error_type: ErrorType::AuthError,
                    description: "bad password".into(),
                },
            );
            // The machine tears down and restarts the conversation on its
            // own after an auth error, so the next submission has a prompt.
            assert_cancel(request(stream));
            respond(stream, Response::Success);
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_answer(request(stream), Some("right"));
            respond(stream, Response::Success);
            assert_start(request(stream));
            respond(stream, Response::Success);
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.start("alice", &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("wrong".into()), &mut ui)
            .unwrap();
        assert!(!machine.is_complete());
        // The failure must still be on screen after the restarted
        // conversation re-prompts, not flash-and-cleared by it.
        assert_eq!(
            ui.0.last(),
            Some(&UiCall::Error("bad password".into())),
            "error must be re-raised after the retry prompt"
        );
        machine
            .handle(UiMessage::Respond("right".into()), &mut ui)
            .unwrap();
        assert!(machine.is_complete());
        assert!(ui.0.contains(&UiCall::Error("bad password".into())));
    }

    #[test]
    fn handles_multiple_prompts() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Visible, "Login:"));
            assert_answer(request(stream), Some("alice@example.com"));
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_answer(request(stream), Some("secret"));
            respond(stream, Response::Success);
            assert_start(request(stream));
            respond(stream, Response::Success);
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.start("alice", &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("alice@example.com".into()), &mut ui)
            .unwrap();
        machine
            .handle(UiMessage::Respond("secret".into()), &mut ui)
            .unwrap();
        let prompts: Vec<_> =
            ui.0.iter()
                .filter(|call| matches!(call, UiCall::Prompt(..)))
                .collect();
        assert_eq!(
            prompts,
            [
                &UiCall::Prompt("Login:".into(), false),
                &UiCall::Prompt("Password:".into(), true)
            ]
        );
    }

    #[test]
    fn displays_info_and_error_mid_conversation() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(
                stream,
                auth_message(AuthMessageType::Info, "Touch your key"),
            );
            assert_answer(request(stream), None);
            respond(
                stream,
                auth_message(AuthMessageType::Error, "Key timed out"),
            );
            assert_answer(request(stream), None);
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_answer(request(stream), Some("secret"));
            respond(stream, Response::Success);
            assert_start(request(stream));
            respond(stream, Response::Success);
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.start("alice", &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("secret".into()), &mut ui)
            .unwrap();
        let messages: Vec<_> =
            ui.0.iter()
                .filter(|call| !matches!(call, UiCall::Busy(_)))
                .collect();
        assert_eq!(
            messages,
            [
                &UiCall::Info("Touch your key".into()),
                &UiCall::Error("Key timed out".into()),
                &UiCall::Prompt("Password:".into(), true)
            ]
        );
    }

    #[test]
    fn selectable_user_skips_username_and_manual_entry_restores_it() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_cancel(request(stream));
            respond(stream, Response::Success);
            assert_create(request(stream), "bob");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_cancel(request(stream));
            respond(stream, Response::Success);
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();

        machine.select_user(Some("alice"), &mut ui).unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password:".into(), true)));
        assert_eq!(machine.user(), Some("alice"));

        machine.select_user(Some("bob"), &mut ui).unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password:".into(), true)));
        assert_eq!(machine.user(), Some("bob"));

        machine.select_user(None, &mut ui).unwrap();
        assert_eq!(
            ui.0.last(),
            Some(&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        assert_eq!(machine.user(), None);
    }

    #[test]
    fn username_stage_asks_before_contacting_greetd() {
        let Some(server) = FakeServer::spawn(|stream| {
            // greetd sees nothing until the username is submitted.
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_answer(request(stream), Some("hunter2"));
            respond(stream, Response::Success);
            assert_start(request(stream));
            respond(stream, Response::Success);
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.begin(None, &mut ui).unwrap();
        assert_eq!(
            ui.0.first(),
            Some(&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        // Empty and whitespace submissions re-prompt without a connection.
        machine
            .handle(UiMessage::Respond("  ".into()), &mut ui)
            .unwrap();
        assert_eq!(
            ui.0.last(),
            Some(&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        machine
            .handle(UiMessage::Respond(" alice ".into()), &mut ui)
            .unwrap();
        machine
            .handle(UiMessage::Respond("hunter2".into()), &mut ui)
            .unwrap();
        assert!(machine.is_complete());
    }

    #[test]
    fn empty_username_submits_default() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
        }) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_default_user(Some("alice".into()));
        let mut ui = RecordingUi::default();
        machine.begin(None, &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("   ".into()), &mut ui)
            .unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password:".into(), true)));
        assert_eq!(machine.user(), Some("alice"));
    }

    #[test]
    fn cancel_returns_to_username_stage() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert_cancel(request(stream));
            respond(stream, Response::Success);
            assert_create(request(stream), "bob");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.begin(None, &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("alice".into()), &mut ui)
            .unwrap();
        machine.handle(UiMessage::Cancel, &mut ui).unwrap();
        assert_eq!(
            ui.0.last(),
            Some(&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        machine
            .handle(UiMessage::Respond("bob".into()), &mut ui)
            .unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password:".into(), true)));
    }

    #[test]
    fn cancels_mid_prompt_and_can_restart() {
        let Some(server) = FakeServer::spawn(|stream| {
            assert_create(request(stream), "alice");
            respond(stream, auth_message(AuthMessageType::Secret, "Password:"));
            assert!(matches!(request(stream), Request::CancelSession));
            respond(stream, Response::Success);
            assert_create(request(stream), "bob");
            respond(stream, auth_message(AuthMessageType::Visible, "Code:"));
        }) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.start("alice", &mut ui).unwrap();
        machine.handle(UiMessage::Cancel, &mut ui).unwrap();
        machine.start("bob", &mut ui).unwrap();
        assert!(!machine.is_complete());
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Code:".into(), false)));
    }
}
