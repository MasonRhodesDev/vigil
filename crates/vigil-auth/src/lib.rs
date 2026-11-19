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

pub fn friendly_error(description: &str) -> String {
    let Some((call, code)) = description.split_once(": ") else {
        return description.to_string();
    };
    if !call.starts_with("pam_") {
        return description.to_string();
    }
    let text = match code.trim() {
        "AUTH_ERR" | "USER_UNKNOWN" => "Incorrect password",
        "MAXTRIES" => "Too many failed attempts. Try again later.",
        "PERM_DENIED" => "Access denied",
        "ACCT_EXPIRED" => "This account has expired",
        "NEW_AUTHTOK_REQD" | "AUTHTOK_EXPIRED" => "Your password has expired",
        "AUTHINFO_UNAVAIL" | "CRED_INSUFFICIENT" | "SERVICE_ERR" | "SYSTEM_ERR" => {
            "Sign-in is unavailable right now"
        }
        _ => "Sign-in failed",
    };
    text.to_string()
}

fn show_greetd_error(ui: &mut dyn AuthUi, description: &str) -> String {
    let text = friendly_error(description);
    eprintln!("vigil: auth: greetd error: {description}");
    ui.show_error(&text);
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    AwaitingUser,
    AwaitingPrompt,
    AwaitingRetry,
    Complete,
}

pub const USERNAME_PROMPT: &str = "Username";
pub const RETRY_PROMPT: &str = "Press Enter to try again";

pub struct AuthMachine {
    stream: UnixStream,
    command: Vec<String>,
    env: Vec<String>,
    state: State,
    user: Option<String>,
    default_user: Option<String>,
    fixed_user: bool,
    answered: bool,
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
            answered: false,
        })
    }

    pub fn set_session(&mut self, command: Vec<String>, env: Vec<String>) {
        self.command = command;
        self.env = env;
    }

    pub fn set_default_user(&mut self, user: Option<String>) {
        self.default_user = user;
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    pub fn is_complete(&self) -> bool {
        self.state == State::Complete
    }

    pub fn awaiting_username(&self) -> bool {
        self.state == State::AwaitingUser
    }

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

    fn await_retry(&mut self, ui: &mut dyn AuthUi, error: &str) {
        ui.show_prompt(RETRY_PROMPT, true);
        ui.show_error(error);
        self.state = State::AwaitingRetry;
    }

    fn restart(&mut self, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        match self.user.clone() {
            Some(user) => self.start(&user, ui),
            None => {
                self.prompt_username(ui);
                Ok(())
            }
        }
    }

    pub fn start(&mut self, user: &str, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        if self.state == State::Complete {
            return Err(AuthError("session has already started".into()));
        }
        if self.state == State::AwaitingPrompt {
            self.cancel(ui)?;
        }
        self.user = Some(user.to_owned());
        self.answered = false;

        let response = self.transact(
            &Request::CreateSession {
                username: user.to_owned(),
            },
            ui,
        )?;
        self.process_response(response, ui)
    }

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
                self.answered = true;
                let response = self.transact(
                    &Request::PostAuthMessageResponse {
                        response: Some(response),
                    },
                    ui,
                )?;
                self.process_response(response, ui)
            }
            UiMessage::Respond(_) if self.state == State::AwaitingRetry => self.restart(ui),
            UiMessage::Cancel if matches!(self.state, State::AwaitingUser | State::Complete) => {
                Ok(())
            }
            UiMessage::Cancel => {
                if self.state == State::AwaitingPrompt {
                    self.cancel(ui)?;
                }
                if self.fixed_user {
                    self.restart(ui)
                } else {
                    self.prompt_username(ui);
                    Ok(())
                }
            }
            UiMessage::Respond(_) => Err(AuthError("no authentication prompt is pending".into())),
            UiMessage::SelectSession(_) | UiMessage::SelectUser(_) => Ok(()),
            UiMessage::Power(_) => Err(AuthError("power actions are not authentication".into())),
        }
    }

    fn cancel(&mut self, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        let response = self.transact(&Request::CancelSession, ui)?;
        self.state = State::Idle;
        match response {
            Response::Success => Ok(()),
            Response::Error { description, .. } => {
                eprintln!("vigil: auth: greetd refused the cancel: {description}");
                Ok(())
            }
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
                    eprintln!(
                        "vigil: [t+{}ms] auth: greetd accepted the credentials, sending start_session",
                        vigil_core::uptime_ms()
                    );
                    let request = Request::StartSession {
                        cmd: self.command.clone(),
                        env: self.env.clone(),
                    };
                    match self.transact(&request, ui)? {
                        Response::Success => {
                            eprintln!(
                                "vigil: [t+{}ms] auth: start_session accepted",
                                vigil_core::uptime_ms()
                            );
                            self.state = State::Complete;
                            return Ok(());
                        }
                        Response::Error { description, .. } => {
                            let text = show_greetd_error(ui, &description);
                            let _ = self.transact(&Request::CancelSession, ui);
                            self.await_retry(ui, &text);
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
                    let text = show_greetd_error(ui, &description);
                    let _ = self.transact(&Request::CancelSession, ui);
                    let Some(user) = self.user.clone().filter(|_| self.answered) else {
                        self.await_retry(ui, &text);
                        return Ok(());
                    };
                    self.answered = false;
                    pending_error = Some(text);
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
    use std::sync::{Arc, Mutex};
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

    fn secret_prompt() -> Response {
        auth_message(AuthMessageType::Secret, "Password:")
    }

    fn refusal(description: &str) -> Response {
        Response::Error {
            error_type: ErrorType::AuthError,
            description: description.into(),
        }
    }

    type Requests = Arc<Mutex<Vec<String>>>;

    fn scripted(replies: Vec<Response>) -> Option<(FakeServer, Requests)> {
        let requests = Requests::default();
        let seen = requests.clone();
        let server = FakeServer::spawn(move |stream| {
            for reply in replies {
                let Ok(request) = Request::read_from(stream) else {
                    return;
                };
                seen.lock().unwrap().push(match request {
                    Request::CreateSession { username } => format!("create {username}"),
                    Request::PostAuthMessageResponse { response } => {
                        format!("answer {}", response.as_deref().unwrap_or("<none>"))
                    }
                    Request::StartSession { .. } => "start".into(),
                    Request::CancelSession => "cancel".into(),
                });
                if reply.write_to(stream).is_err() {
                    return;
                }
            }
        })?;
        Some((server, requests))
    }

    fn sent(requests: &Requests) -> Vec<String> {
        requests.lock().unwrap().clone()
    }

    fn shown(ui: &RecordingUi) -> Vec<&UiCall> {
        ui.0.iter()
            .filter(|call| !matches!(call, UiCall::Busy(_)))
            .collect()
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
    fn pam_codes_become_short_messages() {
        assert_eq!(
            friendly_error("pam_authenticate: AUTH_ERR"),
            "Incorrect password"
        );
        assert_eq!(
            friendly_error("pam_authenticate: USER_UNKNOWN"),
            "Incorrect password"
        );
        assert_eq!(
            friendly_error("pam_authenticate: MAXTRIES"),
            "Too many failed attempts. Try again later."
        );
        assert_eq!(
            friendly_error("pam_acct_mgmt: ACCT_EXPIRED"),
            "This account has expired"
        );
        assert_eq!(
            friendly_error("pam_acct_mgmt: NEW_AUTHTOK_REQD"),
            "Your password has expired"
        );
        assert_eq!(friendly_error("pam_authenticate: WEIRD"), "Sign-in failed");
        assert_eq!(friendly_error("bad password"), "bad password");
        assert_eq!(
            friendly_error("session already started: yes"),
            "session already started: yes"
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

    #[test]
    fn a_fixed_user_cancel_starts_that_users_session_again() {
        let Some((server, requests)) = scripted(vec![
            secret_prompt(),
            Response::Success,
            secret_prompt(),
            Response::Success,
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.begin(Some("alice"), &mut ui).unwrap();
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(
            sent(&requests),
            [
                "create alice",
                "cancel",
                "create alice",
                "answer hunter2",
                "start"
            ]
        );
        assert!(machine.is_complete());
    }

    #[test]
    fn a_session_refused_before_any_question_is_not_recreated_unasked() {
        let mut replies = Vec::new();
        for _ in 0..4 {
            replies.push(refusal("pam_authenticate: AUTH_ERR"));
            replies.push(Response::Success);
        }
        let Some((server, requests)) = scripted(replies) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
        assert_eq!(
            shown(&ui)[1..],
            [
                &UiCall::Prompt(RETRY_PROMPT.into(), true),
                &UiCall::Error("Incorrect password".into())
            ]
        );
    }

    #[test]
    fn acknowledging_a_message_does_not_count_as_an_answer() {
        let mut replies = Vec::new();
        for _ in 0..4 {
            replies.push(auth_message(AuthMessageType::Error, "Logins are closed"));
            replies.push(refusal("pam_authenticate: AUTH_ERR"));
            replies.push(Response::Success);
        }
        let Some((server, requests)) = scripted(replies) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "answer <none>", "cancel"]);
    }

    #[test]
    fn a_wrong_answer_reopens_the_session_once() {
        let mut replies = vec![
            secret_prompt(),
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
        ];
        for _ in 0..4 {
            replies.push(refusal("pam_authenticate: MAXTRIES"));
            replies.push(Response::Success);
        }
        let Some((server, requests)) = scripted(replies) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond("wrong".into()), &mut ui);
        assert_eq!(
            sent(&requests),
            [
                "create alice",
                "answer wrong",
                "cancel",
                "create alice",
                "cancel"
            ]
        );
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Error(
                "Too many failed attempts. Try again later.".into()
            ))
        );
    }

    #[test]
    fn enter_retries_a_refused_session_without_sending_what_was_typed() {
        let Some((server, requests)) = scripted(vec![
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
            secret_prompt(),
            Response::Success,
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.begin(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond("typed blind".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel", "create alice"]);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password:".into(), true))
        );
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert!(machine.is_complete());
    }

    #[test]
    fn cancel_leaves_a_refused_session_for_the_username_stage() {
        let Some((server, requests)) = scripted(vec![
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
            secret_prompt(),
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        assert!(!machine.awaiting_username());
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert!(machine.awaiting_username());
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        let _ = machine.handle(UiMessage::Respond("bob".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel", "create bob"]);
    }

    #[test]
    fn a_failed_start_session_can_be_retried() {
        let Some((server, requests)) = scripted(vec![
            secret_prompt(),
            Response::Success,
            Response::Error {
                error_type: ErrorType::Error,
                description: "no such session".into(),
            },
            Response::Success,
            secret_prompt(),
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(
            shown(&ui)[1..],
            [
                &UiCall::Error("no such session".into()),
                &UiCall::Prompt(RETRY_PROMPT.into(), true),
                &UiCall::Error("no such session".into())
            ]
        );
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert_eq!(
            sent(&requests),
            [
                "create alice",
                "answer hunter2",
                "start",
                "cancel",
                "create alice"
            ]
        );
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password:".into(), true))
        );
    }

    #[test]
    fn a_refused_cancel_still_leaves_a_prompt() {
        let Some((server, requests)) = scripted(vec![
            secret_prompt(),
            refusal("session worker is gone"),
            secret_prompt(),
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert!(machine.awaiting_username());
        let _ = machine.handle(UiMessage::Respond("bob".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel", "create bob"]);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password:".into(), true))
        );
    }
}
