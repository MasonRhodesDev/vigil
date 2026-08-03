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
    AwaitingPrompt,
    Complete,
}

/// One greetd conversation. Connects to the explicitly supplied socket, or
/// falls back to `$GREETD_SOCK` when no path is supplied.
pub struct AuthMachine {
    stream: UnixStream,
    command: Vec<String>,
    env: Vec<String>,
    state: State,
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
        })
    }

    /// Set the command and environment passed verbatim to greetd when
    /// authentication succeeds.
    pub fn set_session(&mut self, command: Vec<String>, env: Vec<String>) {
        self.command = command;
        self.env = env;
    }

    /// Whether greetd accepted `start_session` and the greeter may finish.
    pub fn is_complete(&self) -> bool {
        self.state == State::Complete
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
            UiMessage::Respond(response) if self.state == State::AwaitingPrompt => {
                let response = self.transact(
                    &Request::PostAuthMessageResponse {
                        response: Some(response),
                    },
                    ui,
                )?;
                self.process_response(response, ui)
            }
            UiMessage::Cancel if self.state == State::AwaitingPrompt => self.cancel(ui),
            UiMessage::Respond(_) => Err(AuthError("no authentication prompt is pending".into())),
            UiMessage::Cancel => Ok(()),
            UiMessage::SelectSession(_) => {
                Err(AuthError("session selection is not implemented yet".into()))
            }
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
        loop {
            match response {
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Visible,
                    auth_message,
                } => {
                    ui.show_prompt(&auth_message, false);
                    self.state = State::AwaitingPrompt;
                    return Ok(());
                }
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message,
                } => {
                    ui.show_prompt(&auth_message, true);
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
                    return Ok(());
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
        machine.start("alice", &mut ui).unwrap();
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
