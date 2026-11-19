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
    AwaitingCommit,
    AwaitingPrompt,
    Complete,
}

pub const USERNAME_PROMPT: &str = "Username";
pub const PASSWORD_PROMPT: &str = "Password";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Prompt {
    text: String,
    secret: bool,
}

pub struct AuthMachine {
    stream: UnixStream,
    command: Vec<String>,
    env: Vec<String>,
    state: State,
    user: Option<String>,
    default_user: Option<String>,
    fixed_user: bool,
    typed: bool,
    asked: bool,
    unprompted: Option<String>,
    first_question: Option<(String, Prompt)>,
    stage: String,
    stage_key: String,
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
            typed: false,
            asked: false,
            unprompted: None,
            first_question: None,
            stage: String::new(),
            stage_key: String::new(),
        })
    }

    pub fn set_session(&mut self, command: Vec<String>, env: Vec<String>) {
        self.command = command;
        self.env = env;
    }

    pub fn set_default_user(&mut self, user: Option<String>) {
        self.default_user = user;
    }

    pub fn set_unprompted(&mut self, user: Option<String>) {
        self.unprompted = user;
    }

    pub fn unprompted(&self) -> Option<&str> {
        self.unprompted.as_deref()
    }

    pub fn asked(&self) -> bool {
        self.asked
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

    pub fn stage(&self) -> &str {
        &self.stage
    }

    fn logged_user(&self) -> &str {
        match &self.user {
            Some(_) if self.typed => "<typed>",
            Some(user) => user,
            None => "<none>",
        }
    }

    fn log_stage(&mut self, prompt: &Prompt, local: bool) {
        let kind = if prompt.secret { "secret" } else { "visible" };
        let origin = if local { " local" } else { "" };
        let user = self.logged_user().to_owned();
        let key = format!("user={user} prompt={:?} {kind}{origin}", prompt.text);
        if key == self.stage_key {
            return;
        }
        let own = prompt.text == USERNAME_PROMPT || prompt.text == PASSWORD_PROMPT;
        let shown = if self.typed && !own {
            "<hidden>".to_owned()
        } else {
            format!("{:?}", prompt.text)
        };
        self.stage = format!("user={user} prompt={shown} {kind}{origin}");
        self.stage_key = key;
        eprintln!("vigil: auth stage {}", self.stage);
    }

    fn show_error(&mut self, ui: &mut dyn AuthUi, description: &str) -> String {
        self.stage.clear();
        self.stage_key.clear();
        show_greetd_error(ui, description)
    }

    pub fn begin(&mut self, user: Option<&str>, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        self.fixed_user = user.is_some();
        self.choose(user, false, ui)
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
        self.close(ui)?;
        self.choose(user, false, ui)
    }

    fn choose(
        &mut self,
        user: Option<&str>,
        typed: bool,
        ui: &mut dyn AuthUi,
    ) -> Result<(), AuthError> {
        let Some(user) = user else {
            self.prompt_username(ui);
            return Ok(());
        };
        self.user = Some(user.to_owned());
        self.typed = typed;
        if self.unprompted.as_deref() == Some(user) {
            eprintln!("vigil: auth unasked user={}", self.logged_user());
            ui.reset_input();
            return self.open(None, ui);
        }
        self.offer(ui);
        Ok(())
    }

    fn prompt_username(&mut self, ui: &mut dyn AuthUi) {
        self.user = None;
        self.typed = false;
        ui.reset_input();
        ui.show_prompt(USERNAME_PROMPT, false);
        self.state = State::AwaitingUser;
        let prompt = Prompt {
            text: USERNAME_PROMPT.into(),
            secret: false,
        };
        self.log_stage(&prompt, true);
    }

    fn offered(&self) -> Prompt {
        match (&self.first_question, &self.user) {
            (Some((user, prompt)), Some(current)) if user == current => prompt.clone(),
            _ => Prompt {
                text: PASSWORD_PROMPT.into(),
                secret: true,
            },
        }
    }

    fn offer(&mut self, ui: &mut dyn AuthUi) {
        let prompt = self.offered();
        ui.reset_input();
        ui.show_prompt(&prompt.text, prompt.secret);
        self.state = State::AwaitingCommit;
        self.log_stage(&prompt, true);
    }

    // greetd can end a conversation that has asked a question only with an
    // answer or with a failure PAM counts, so none is opened before a submit.
    fn open(&mut self, answer: Option<String>, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        let Some(user) = self.user.clone() else {
            self.prompt_username(ui);
            return Ok(());
        };
        eprintln!("vigil: auth open user={}", self.logged_user());
        self.asked = false;
        let response = self.transact(&Request::CreateSession { username: user }, ui)?;
        self.process_response(response, answer, ui)
    }

    pub fn handle(&mut self, msg: UiMessage, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        match msg {
            UiMessage::Respond(text) => match self.state {
                State::AwaitingUser => {
                    let name = text.trim();
                    if name.is_empty() {
                        let default = self.default_user.clone();
                        self.choose(default.as_deref(), false, ui)
                    } else {
                        let typed = self.default_user.as_deref() != Some(name);
                        self.choose(Some(name), typed, ui)
                    }
                }
                State::AwaitingCommit => self.open(Some(text).filter(|text| !text.is_empty()), ui),
                State::AwaitingPrompt => {
                    let response = self.transact(
                        &Request::PostAuthMessageResponse {
                            response: Some(text),
                        },
                        ui,
                    )?;
                    self.process_response(response, None, ui)
                }
                State::Idle | State::Complete => {
                    Err(AuthError("no authentication prompt is pending".into()))
                }
            },
            UiMessage::Cancel if matches!(self.state, State::AwaitingUser | State::Complete) => {
                Ok(())
            }
            UiMessage::Cancel => {
                self.close(ui)?;
                if self.fixed_user {
                    eprintln!("vigil: auth cancel -> restart user={}", self.logged_user());
                    self.offer(ui);
                } else {
                    eprintln!("vigil: auth cancel -> username");
                    self.prompt_username(ui);
                }
                Ok(())
            }
            UiMessage::SelectSession(_) | UiMessage::SelectUser(_) => Ok(()),
            UiMessage::Power(_) => Err(AuthError("power actions are not authentication".into())),
        }
    }

    fn close(&mut self, ui: &mut dyn AuthUi) -> Result<(), AuthError> {
        if self.state != State::AwaitingPrompt {
            return Ok(());
        }
        eprintln!("vigil: auth close user={} unanswered", self.logged_user());
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

    fn fail(&mut self, ui: &mut dyn AuthUi, description: &str) {
        let text = self.show_error(ui, description);
        let _ = self.transact(&Request::CancelSession, ui);
        self.offer(ui);
        ui.show_error(&text);
    }

    fn process_response(
        &mut self,
        mut response: Response,
        mut answer: Option<String>,
        ui: &mut dyn AuthUi,
    ) -> Result<(), AuthError> {
        loop {
            match response {
                Response::AuthMessage {
                    auth_message_type: kind @ (AuthMessageType::Visible | AuthMessageType::Secret),
                    auth_message,
                } => {
                    let prompt = Prompt {
                        text: auth_message,
                        secret: matches!(kind, AuthMessageType::Secret),
                    };
                    let offered = self.offered();
                    if !std::mem::replace(&mut self.asked, true) {
                        self.first_question = self.user.clone().map(|user| (user, prompt.clone()));
                    }
                    if self.unprompted.is_some() && self.unprompted == self.user {
                        eprintln!(
                            "vigil: auth: user={} was asked a question; no longer opening unasked",
                            self.logged_user()
                        );
                        self.unprompted = None;
                    }
                    if let Some(answer) = answer.take().filter(|_| prompt.secret == offered.secret)
                    {
                        response = self.transact(
                            &Request::PostAuthMessageResponse {
                                response: Some(answer),
                            },
                            ui,
                        )?;
                        continue;
                    }
                    ui.show_prompt(&prompt.text, prompt.secret);
                    self.state = State::AwaitingPrompt;
                    self.log_stage(&prompt, false);
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
                            self.fail(ui, &description);
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
                    self.fail(ui, &description);
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
        Reset,
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
        fn reset_input(&mut self) {
            self.0.push(UiCall::Reset);
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
            .filter(|call| !matches!(call, UiCall::Busy(_) | UiCall::Reset))
            .collect()
    }

    fn fields(ui: &RecordingUi) -> Vec<String> {
        ui.0.iter()
            .filter_map(|call| match call {
                UiCall::Reset => Some("reset".to_owned()),
                UiCall::Prompt(text, _) => Some(text.clone()),
                _ => None,
            })
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
        machine.select_user(Some("alice"), &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("hunter2".into()), &mut ui)
            .unwrap();
        assert!(machine.is_complete());
        assert_eq!(shown(&ui), [&UiCall::Prompt("Password".into(), true)]);
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
    fn escape_before_any_submit_sends_greetd_nothing() {
        let Some((server, requests)) = scripted(vec![secret_prompt(), Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert!(machine.awaiting_username());
        drop(machine);
        drop(server);
        assert_eq!(sent(&requests), [""; 0]);
    }

    #[test]
    fn a_greeter_that_stops_before_any_submit_sends_greetd_nothing() {
        let Some((server, requests)) = scripted(vec![secret_prompt(), Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        drop(machine);
        drop(server);
        assert_eq!(sent(&requests), [""; 0]);
    }

    #[test]
    fn a_wrong_password_is_one_attempt_and_opens_nothing_after_it() {
        let Some((server, requests)) = scripted(vec![
            secret_prompt(),
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
            secret_prompt(),
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond("wrong".into()), &mut ui);
        assert_eq!(
            shown(&ui)[2..],
            [
                &UiCall::Prompt("Password:".into(), true),
                &UiCall::Error("Incorrect password".into())
            ]
        );
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        drop(machine);
        drop(server);
        assert_eq!(
            sent(&requests),
            ["create alice", "answer wrong", "cancel"],
            "one conversation, one answer, and the cancel that clears the failed session"
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
        machine.select_user(Some("alice"), &mut ui).unwrap();
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
    fn enter_with_nothing_typed_asks_pam_without_answering() {
        let Some((server, requests)) =
            scripted(vec![secret_prompt(), Response::Success, Response::Success])
        else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert_eq!(sent(&requests), ["create alice"]);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password:".into(), true))
        );
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "answer hunter2", "start"]);
        assert!(machine.is_complete());
    }

    #[test]
    fn handles_multiple_prompts() {
        let Some((server, requests)) = scripted(vec![
            auth_message(AuthMessageType::Visible, "Login:"),
            secret_prompt(),
            Response::Success,
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        let _ = machine.handle(UiMessage::Respond("alice@example.com".into()), &mut ui);
        let _ = machine.handle(UiMessage::Respond("secret".into()), &mut ui);
        assert_eq!(
            sent(&requests),
            [
                "create alice",
                "answer alice@example.com",
                "answer secret",
                "start"
            ]
        );
        assert_eq!(
            shown(&ui),
            [
                &UiCall::Prompt("Password".into(), true),
                &UiCall::Prompt("Login:".into(), false),
                &UiCall::Prompt("Password:".into(), true)
            ]
        );
        assert!(machine.is_complete());
    }

    #[test]
    fn a_first_question_of_another_kind_is_shown_instead_of_answered() {
        let Some((server, requests)) = scripted(vec![
            auth_message(AuthMessageType::Visible, "Code:"),
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
            auth_message(AuthMessageType::Visible, "Code:"),
            Response::Success,
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond("my password".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice"]);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Code:".into(), false))
        );
        let _ = machine.handle(UiMessage::Respond("111111".into()), &mut ui);
        assert_eq!(
            shown(&ui)[shown(&ui).len() - 2..],
            [
                &UiCall::Prompt("Code:".into(), false),
                &UiCall::Error("Incorrect password".into())
            ],
            "the next attempt is offered under the question PAM asks first"
        );
        let _ = machine.handle(UiMessage::Respond("222222".into()), &mut ui);
        assert_eq!(
            sent(&requests),
            [
                "create alice",
                "answer 111111",
                "cancel",
                "create alice",
                "answer 222222",
                "start"
            ]
        );
        assert!(machine.is_complete());
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
        machine.select_user(Some("alice"), &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("secret".into()), &mut ui)
            .unwrap();
        assert!(machine.is_complete());
        assert_eq!(
            shown(&ui),
            [
                &UiCall::Prompt("Password".into(), true),
                &UiCall::Info("Touch your key".into()),
                &UiCall::Error("Key timed out".into())
            ]
        );
    }

    #[test]
    fn selectable_user_skips_username_and_manual_entry_restores_it() {
        let Some((server, requests)) = scripted(vec![secret_prompt(), Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();

        machine.select_user(Some("alice"), &mut ui).unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password".into(), true)));
        assert_eq!(machine.user(), Some("alice"));

        machine.select_user(Some("bob"), &mut ui).unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password".into(), true)));
        assert_eq!(machine.user(), Some("bob"));

        machine.select_user(None, &mut ui).unwrap();
        assert_eq!(
            ui.0.last(),
            Some(&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        assert_eq!(machine.user(), None);
        drop(machine);
        drop(server);
        assert_eq!(sent(&requests), [""; 0]);
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
            shown(&ui).first(),
            Some(&&UiCall::Prompt(USERNAME_PROMPT.into(), false))
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
        let Some((server, requests)) = scripted(vec![secret_prompt(), Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_default_user(Some("alice".into()));
        let mut ui = RecordingUi::default();
        machine.begin(None, &mut ui).unwrap();
        machine
            .handle(UiMessage::Respond("   ".into()), &mut ui)
            .unwrap();
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password".into(), true)));
        assert_eq!(machine.user(), Some("alice"));
        drop(machine);
        drop(server);
        assert_eq!(sent(&requests), [""; 0]);
    }

    #[test]
    fn cancel_returns_to_username_stage() {
        let Some((server, requests)) = scripted(vec![secret_prompt(), Response::Success]) else {
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
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Password".into(), true)));
        drop(machine);
        drop(server);
        assert_eq!(sent(&requests), [""; 0]);
    }

    #[test]
    fn cancels_mid_prompt_and_can_restart() {
        let Some((server, requests)) = scripted(vec![
            secret_prompt(),
            Response::Success,
            auth_message(AuthMessageType::Visible, "Code:"),
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
        let _ = machine.select_user(Some("bob"), &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert!(!machine.is_complete());
        assert_eq!(sent(&requests), ["create alice", "cancel", "create bob"]);
        assert_eq!(ui.0.last(), Some(&UiCall::Prompt("Code:".into(), false)));
    }

    #[test]
    fn a_fixed_user_cancel_offers_that_user_again() {
        let Some((server, requests)) =
            scripted(vec![secret_prompt(), Response::Success, Response::Success])
        else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        machine.begin(Some("alice"), &mut ui).unwrap();
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert!(!machine.awaiting_username());
        assert_eq!(
            shown(&ui),
            [
                &UiCall::Prompt("Password".into(), true),
                &UiCall::Prompt("Password".into(), true)
            ]
        );
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "answer hunter2", "start"]);
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
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
        assert_eq!(
            shown(&ui)[2..],
            [
                &UiCall::Prompt("Password".into(), true),
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
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "answer <none>", "cancel"]);
    }

    #[test]
    fn text_typed_for_a_session_refused_before_any_question_is_never_sent() {
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
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
        assert_eq!(
            shown(&ui)[2..],
            [
                &UiCall::Prompt("Password".into(), true),
                &UiCall::Error("Incorrect password".into())
            ]
        );
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
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert!(!machine.awaiting_username());
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert!(machine.awaiting_username());
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt(USERNAME_PROMPT.into(), false))
        );
        let _ = machine.handle(UiMessage::Respond("bob".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
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
            Response::Success,
            Response::Success,
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
                &UiCall::Prompt("Password:".into(), true),
                &UiCall::Error("no such session".into())
            ]
        );
        assert_eq!(
            sent(&requests),
            ["create alice", "answer hunter2", "start", "cancel"]
        );
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(
            sent(&requests),
            [
                "create alice",
                "answer hunter2",
                "start",
                "cancel",
                "create alice",
                "answer hunter2",
                "start"
            ]
        );
        assert!(machine.is_complete());
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
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert!(machine.awaiting_username());
        let _ = machine.handle(UiMessage::Respond("bob".into()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "cancel"]);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password".into(), true))
        );
    }

    #[test]
    fn a_local_field_is_cleared_each_time_it_is_offered() {
        let Some((server, _requests)) = scripted(vec![
            secret_prompt(),
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.begin(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        let _ = machine.handle(UiMessage::Respond("wrong".into()), &mut ui);
        let _ = machine.select_user(Some("bob"), &mut ui);
        let _ = machine.select_user(None, &mut ui);
        assert_eq!(
            fields(&ui),
            [
                "reset",
                "Password",
                "reset",
                "Password",
                "reset",
                "Password:",
                "reset",
                "Password",
                "reset",
                "Username"
            ]
        );
    }

    #[test]
    fn a_question_from_pam_leaves_the_field_as_it_is() {
        let Some((server, _requests)) = scripted(vec![
            secret_prompt(),
            auth_message(AuthMessageType::Visible, "Code:"),
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(fields(&ui), ["reset", "Password", "Password:", "Code:"]);
    }

    #[test]
    fn the_stage_names_the_selected_user_and_the_prompt() {
        let Some((server, _requests)) = scripted(vec![secret_prompt(), Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        assert_eq!(
            machine.stage(),
            "user=alice prompt=\"Password\" secret local"
        );
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert_eq!(machine.stage(), "user=alice prompt=\"Password:\" secret");
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        assert_eq!(
            machine.stage(),
            "user=<none> prompt=\"Username\" visible local"
        );
    }

    #[test]
    fn a_prompt_that_echoes_a_typed_name_never_reaches_the_stage_line() {
        let Some((server, _requests)) = scripted(vec![
            auth_message(AuthMessageType::Secret, "hunter2's password:"),
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        let mut ui = RecordingUi::default();
        let _ = machine.begin(None, &mut ui);
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert!(!machine.stage().contains("hunter2"), "{}", machine.stage());
        assert_eq!(machine.stage(), "user=<typed> prompt=<hidden> secret");
        let _ = machine.handle(UiMessage::Respond("wrong".into()), &mut ui);
        assert!(!machine.stage().contains("hunter2"), "{}", machine.stage());
        assert_eq!(machine.stage(), "user=<typed> prompt=<hidden> secret local");
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Error("Incorrect password".into()))
        );
    }

    #[test]
    fn a_typed_name_never_reaches_the_stage_line() {
        let Some((server, requests)) = scripted(vec![
            secret_prompt(),
            refusal("pam_authenticate: AUTH_ERR"),
            Response::Success,
        ]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_default_user(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.begin(None, &mut ui);
        let _ = machine.handle(UiMessage::Respond("hunter2".into()), &mut ui);
        assert_eq!(
            machine.stage(),
            "user=<typed> prompt=\"Password\" secret local"
        );
        let _ = machine.handle(UiMessage::Respond("again".into()), &mut ui);
        assert_eq!(machine.stage(), "user=<typed> prompt=<hidden> secret local");
        assert_eq!(sent(&requests)[0], "create hunter2");
    }

    #[test]
    fn the_default_user_taken_by_an_empty_answer_is_named() {
        let Some((server, _requests)) = scripted(vec![secret_prompt()]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_default_user(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.begin(None, &mut ui);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert_eq!(
            machine.stage(),
            "user=alice prompt=\"Password\" secret local"
        );
    }

    #[test]
    fn a_user_whose_last_sign_in_asked_nothing_is_signed_in_unasked() {
        let Some((server, requests)) = scripted(vec![Response::Success, Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_unprompted(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.begin(Some("alice"), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "start"]);
        assert!(machine.is_complete());
        assert!(!machine.asked());
    }

    #[test]
    fn enter_at_the_name_stage_signs_the_unprompted_default_user_in() {
        let Some((server, requests)) = scripted(vec![Response::Success, Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_default_user(Some("alice".into()));
        machine.set_unprompted(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.begin(None, &mut ui);
        assert_eq!(sent(&requests), [""; 0]);
        let _ = machine.handle(UiMessage::Respond(String::new()), &mut ui);
        assert_eq!(sent(&requests), ["create alice", "start"]);
        assert!(machine.is_complete());
    }

    #[test]
    fn a_question_put_to_the_unprompted_user_starts_with_an_empty_field() {
        let Some((server, _requests)) = scripted(vec![secret_prompt()]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_unprompted(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("bob"), &mut ui);
        let _ = machine.select_user(Some("alice"), &mut ui);
        assert_eq!(fields(&ui), ["reset", "Password", "reset", "Password:"]);
    }

    #[test]
    fn only_the_unprompted_user_is_opened_unasked() {
        let Some((server, requests)) = scripted(vec![Response::Success, Response::Success]) else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_unprompted(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("bob"), &mut ui);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password".into(), true))
        );
        drop(machine);
        drop(server);
        assert_eq!(sent(&requests), [""; 0]);
    }

    #[test]
    fn a_question_withdraws_the_expectation_of_an_unasked_sign_in() {
        let Some((server, requests)) =
            scripted(vec![secret_prompt(), Response::Success, secret_prompt()])
        else {
            return;
        };
        let mut machine = machine(&server);
        machine.set_unprompted(Some("alice".into()));
        let mut ui = RecordingUi::default();
        let _ = machine.select_user(Some("alice"), &mut ui);
        assert_eq!(
            shown(&ui).last(),
            Some(&&UiCall::Prompt("Password:".into(), true))
        );
        assert_eq!(machine.unprompted(), None);
        assert!(machine.asked());
        let _ = machine.handle(UiMessage::Cancel, &mut ui);
        let _ = machine.select_user(Some("alice"), &mut ui);
        drop(machine);
        drop(server);
        assert_eq!(
            sent(&requests),
            ["create alice", "cancel"],
            "the second selection waits for a submit"
        );
    }
}
