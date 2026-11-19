use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use vigil_auth::{AuthMachine, Prompt};
use vigil_core::{AuthUi, UiMessage};

const USERS: [&str; 3] = ["alice", "bob", "carol"];
const MARK: &str = "Zq7typed";
const ECHO: &str = "{user}";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Visible,
    Secret,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Message(Kind, String),
    Success,
    Refuse(String),
    Frame(Vec<u8>),
    Claim(u32),
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Begin(Option<usize>),
    Pick(Option<usize>),
    Type(String),
    TypeDefaultName,
    Enter,
    Escape,
    Session(Vec<String>),
}

#[derive(Debug, Clone)]
struct Case {
    conforming: bool,
    default_user: Option<usize>,
    unprompted: Option<usize>,
    remembered: Vec<(usize, String, bool)>,
    ops: Vec<Op>,
    replies: Vec<Reply>,
    starts: Vec<bool>,
    refuse_cancels: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Req {
    Create(String),
    Answer(Option<String>),
    Start(Vec<String>),
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
enum Shown {
    Prompt(String, bool),
    Info(String),
    Error(String),
    Busy(bool),
    Reset,
}

#[derive(Debug, Clone)]
enum Event {
    Op(Op),
    Request(Req),
    Reply(Reply),
    Ui(Shown),
    Log(String),
    Returned {
        error: Option<String>,
        complete: bool,
    },
}

type Trace = Arc<Mutex<Vec<Event>>>;

struct Ui(Trace);

impl Ui {
    fn push(&self, shown: Shown) {
        self.0.lock().unwrap().push(Event::Ui(shown));
    }
}

impl AuthUi for Ui {
    fn show_prompt(&mut self, text: &str, secret: bool) {
        self.push(Shown::Prompt(text.into(), secret));
    }
    fn show_info(&mut self, text: &str) {
        self.push(Shown::Info(text.into()));
    }
    fn show_error(&mut self, text: &str) {
        self.push(Shown::Error(text.into()));
    }
    fn set_busy(&mut self, busy: bool) {
        self.push(Shown::Busy(busy));
    }
    fn reset_input(&mut self) {
        self.push(Shown::Reset);
    }
}

struct Journal {
    file: File,
    saved: OwnedFd,
    read: u64,
}

impl Journal {
    fn open(dir: &std::path::Path) -> Self {
        let path = dir.join("stderr");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open the stderr capture");
        let saved = std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .expect("save stderr");
        // SAFETY: both descriptors are open and owned by this process.
        assert_eq!(unsafe { libc::dup2(file.as_raw_fd(), 2) }, 2);
        Self {
            file,
            saved,
            read: 0,
        }
    }

    fn take(&mut self) -> String {
        let end = self.file.metadata().expect("stat the capture").len();
        let mut bytes = vec![0; (end - self.read) as usize];
        self.file
            .read_exact_at(&mut bytes, self.read)
            .expect("read the capture");
        self.read = end;
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // SAFETY: `saved` is the stderr this process started with.
        unsafe { libc::dup2(self.saved.as_raw_fd(), 2) };
    }
}

struct Greetd {
    dir: PathBuf,
    listener: UnixListener,
}

impl Greetd {
    fn bind() -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("vigil-attack-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let listener = UnixListener::bind(dir.join("greetd.sock"))?;
        Ok(Self { dir, listener })
    }

    fn serve(&self, case: &Case, trace: Trace) -> std::thread::JoinHandle<()> {
        let listener = self.listener.try_clone().expect("clone the listener");
        let case = case.clone();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut replies = case.replies.into_iter();
            let mut starts = case.starts.into_iter();
            let mut user = String::new();
            while let Ok(request) = Request::read_from(&mut stream) {
                let request = match request {
                    Request::CreateSession { username } => {
                        user.clone_from(&username);
                        Req::Create(username)
                    }
                    Request::PostAuthMessageResponse { response } => Req::Answer(response),
                    Request::StartSession { cmd, .. } => Req::Start(cmd),
                    Request::CancelSession => Req::Cancel,
                };
                let scripted = match (&request, case.conforming) {
                    (Req::Cancel, true) if case.refuse_cancels => {
                        Some(Reply::Refuse("no session to cancel".into()))
                    }
                    (Req::Cancel, true) => Some(Reply::Success),
                    (Req::Start(_), true) => Some(if starts.next().unwrap_or(true) {
                        Reply::Success
                    } else {
                        Reply::Refuse("session worker: could not start".into())
                    }),
                    (_, true) => Some(
                        replies
                            .next()
                            .unwrap_or_else(|| Reply::Refuse("pam_authenticate: AUTH_ERR".into())),
                    ),
                    (_, false) => replies.next(),
                };
                trace.lock().unwrap().push(Event::Request(request));
                let Some(reply) = scripted else {
                    break;
                };
                let reply = match reply {
                    Reply::Message(kind, text) => Reply::Message(kind, text.replace(ECHO, &user)),
                    Reply::Refuse(text) => Reply::Refuse(text.replace(ECHO, &user)),
                    other => other,
                };
                trace.lock().unwrap().push(Event::Reply(decoded(&reply)));
                if send(&mut stream, reply).is_err() {
                    break;
                }
            }
        })
    }
}

impl Drop for Greetd {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn decoded(reply: &Reply) -> Reply {
    let Reply::Frame(body) = reply else {
        return reply.clone();
    };
    match serde_json::from_slice(body) {
        Ok(Response::Success) => Reply::Success,
        Ok(Response::Error { description, .. }) => Reply::Refuse(description),
        Ok(Response::AuthMessage {
            auth_message_type,
            auth_message,
        }) => {
            let kind = match auth_message_type {
                AuthMessageType::Visible => Kind::Visible,
                AuthMessageType::Secret => Kind::Secret,
                AuthMessageType::Info => Kind::Info,
                AuthMessageType::Error => Kind::Error,
            };
            Reply::Message(kind, auth_message)
        }
        Err(_) => reply.clone(),
    }
}

fn send(stream: &mut UnixStream, reply: Reply) -> std::io::Result<()> {
    let response = match reply {
        Reply::Message(kind, text) => Response::AuthMessage {
            auth_message_type: match kind {
                Kind::Visible => AuthMessageType::Visible,
                Kind::Secret => AuthMessageType::Secret,
                Kind::Info => AuthMessageType::Info,
                Kind::Error => AuthMessageType::Error,
            },
            auth_message: text,
        },
        Reply::Success => Response::Success,
        Reply::Refuse(description) => Response::Error {
            error_type: ErrorType::AuthError,
            description,
        },
        Reply::Frame(body) => {
            stream.write_all(&(body.len() as u32).to_ne_bytes())?;
            return stream.write_all(&body);
        }
        Reply::Claim(len) => return stream.write_all(&len.to_ne_bytes()),
    };
    response.write_to(stream).map_err(std::io::Error::other)
}

fn drive(greetd: &Greetd, case: &Case) -> Vec<Event> {
    let trace = Trace::default();
    let server = greetd.serve(case, trace.clone());
    let socket = greetd.dir.join("greetd.sock");
    let mut machine =
        AuthMachine::connect(socket.to_str()).expect("connect to the scripted greetd");
    let mut ui = Ui(trace.clone());
    let mut journal = Journal::open(&greetd.dir);
    machine.set_session(vec!["session".into()], Vec::new());
    machine.set_default_user(case.default_user.map(|user| USERS[user].to_owned()));
    machine.set_unprompted(case.unprompted.map(|user| USERS[user].to_owned()));
    machine.set_first_questions(
        case.remembered
            .iter()
            .map(|(user, text, secret)| {
                let prompt = Prompt {
                    text: text.clone(),
                    secret: *secret,
                };
                (USERS[*user].to_owned(), prompt)
            })
            .collect(),
    );
    for op in &case.ops {
        trace.lock().unwrap().push(Event::Op(op.clone()));
        let name = |user: &Option<usize>| user.map(|user| USERS[user]);
        let result = match op {
            Op::Begin(user) => machine.begin(name(user), &mut ui),
            Op::Pick(user) => machine.select_user(name(user), &mut ui),
            Op::Type(text) => machine.handle(UiMessage::Respond(text.clone()), &mut ui),
            Op::TypeDefaultName => {
                let text = name(&case.default_user).unwrap_or_default().to_owned();
                machine.handle(UiMessage::Respond(text), &mut ui)
            }
            Op::Enter => machine.handle(UiMessage::Respond(String::new()), &mut ui),
            Op::Escape => machine.handle(UiMessage::Cancel, &mut ui),
            Op::Session(command) => {
                machine.set_session(command.clone(), Vec::new());
                machine.handle(UiMessage::SelectSession(0), &mut ui)
            }
        };
        let mut events = trace.lock().unwrap();
        events.push(Event::Log(journal.take()));
        events.push(Event::Returned {
            error: result.err().map(|error| error.to_string()),
            complete: machine.is_complete(),
        });
    }
    drop((journal, machine, ui));
    server.join().expect("the scripted greetd panicked");
    Arc::try_unwrap(trace).unwrap().into_inner().unwrap()
}

type Question = (String, bool);

fn displayed(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn is_plain_password((text, secret): &Question) -> bool {
    *secret && text.trim_end_matches(|c: char| c == ':' || c.is_whitespace()) == "Password"
}

fn field_shows(field: &Question, question: &Question) -> bool {
    field.1 == question.1
        && (field.0 == question.0
            || field.0 == displayed(&question.0)
            || (*field == ("Password".to_owned(), true) && is_plain_password(question)))
}

fn markers(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices(MARK)
        .filter_map(move |(at, _)| text.get(at..at + MARK.len() + 4))
}

#[derive(Default)]
struct Step {
    op: Option<Op>,
    text: Option<String>,
    field: Option<Question>,
    waiting: Option<Question>,
    requests: Vec<Req>,
    answers: usize,
    log: String,
}

fn check(case: &Case, trace: &[Event]) -> Result<(), String> {
    let unprompted = case.unprompted.map(|user| USERS[user]);
    let default_user = case.default_user.map(|user| USERS[user]);
    let mut field: Option<Question> = None;
    let mut waiting: Option<Question> = None;
    let mut asked: Option<Question> = None;
    let mut granted = false;
    let mut ack_due = false;
    let mut started = false;
    let mut submitted = false;
    let mut busy = false;
    let mut session = vec!["session".to_owned()];
    let mut typed: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut heard: Vec<String> = Vec::new();
    let mut last_request: Option<Req> = None;
    let mut step = Step::default();

    for event in trace {
        match event {
            Event::Op(op) => {
                let text = match op {
                    Op::Type(text) => Some(text.clone()),
                    Op::TypeDefaultName => Some(default_user.unwrap_or_default().to_owned()),
                    Op::Enter => Some(String::new()),
                    _ => None,
                };
                if let Op::Session(command) = op {
                    session.clone_from(command);
                }
                if let Op::Type(text) = op {
                    typed.extend(markers(text).map(str::to_owned));
                    if field == Some(("Username".to_owned(), false)) {
                        names.push(text.trim().to_owned());
                    }
                }
                submitted |= text.is_some();
                step = Step {
                    op: Some(op.clone()),
                    text,
                    field: field.clone(),
                    waiting: waiting.clone(),
                    ..Step::default()
                };
            }
            Event::Request(request) => {
                if started {
                    return Err(format!("{request:?} was sent after the session started"));
                }
                match request {
                    Req::Create(name) => {
                        let unasked = matches!(
                            &step.op,
                            Some(Op::Begin(Some(user)) | Op::Pick(Some(user)))
                                if unprompted == Some(USERS[*user]) && name == USERS[*user]
                        );
                        if step.text.is_none() && !unasked {
                            return Err(format!(
                                "create_session for {name:?} without a submit (submitted before: {submitted})"
                            ));
                        }
                        if case.conforming && waiting.is_some() {
                            return Err(
                                "create_session while greetd still waits on a question".into()
                            );
                        }
                        if markers(name).next().is_some() && !names.contains(name) {
                            return Err(format!(
                                "typed text {name:?} became a user name though the field was not the name field"
                            ));
                        }
                        asked = None;
                    }
                    Req::Answer(Some(text)) => {
                        if step.text.as_ref() != Some(text) {
                            return Err(format!(
                                "{text:?} was sent though this step submitted {:?}",
                                step.text
                            ));
                        }
                        step.answers += 1;
                        if step.answers > 1 {
                            return Err("one submit answered two questions".into());
                        }
                        let question = if case.conforming { &waiting } else { &asked };
                        let Some(question) = question else {
                            return Err("typed text was sent with no question pending".into());
                        };
                        let Some(field) = &step.field else {
                            return Err("typed text was sent though no field was shown".into());
                        };
                        if !field_shows(field, question) {
                            return Err(format!(
                                "text typed under {field:?} was sent as the answer to {question:?}"
                            ));
                        }
                    }
                    Req::Answer(None) => {
                        if !ack_due {
                            return Err("an empty answer went to something that was not an info or error message".into());
                        }
                    }
                    Req::Start(command) => {
                        if !granted {
                            return Err("start_session without a preceding success".into());
                        }
                        if *command != session {
                            return Err(format!(
                                "start_session ran {command:?}, the selected session is {session:?}"
                            ));
                        }
                    }
                    Req::Cancel => {}
                }
                step.requests.push(request.clone());
                last_request = Some(request.clone());
            }
            Event::Reply(reply) => {
                let conversing = matches!(last_request, Some(Req::Create(_) | Req::Answer(_)));
                ack_due = false;
                granted = false;
                if let Reply::Refuse(text) = reply {
                    heard.push(text.clone());
                }
                match (reply, &last_request) {
                    (Reply::Frame(_) | Reply::Claim(_), _) => {}
                    (Reply::Message(kind, text), _) if conversing => {
                        heard.push(text.clone());
                        waiting = None;
                        match kind {
                            Kind::Visible | Kind::Secret => {
                                let question = (text.clone(), *kind == Kind::Secret);
                                waiting = Some(question.clone());
                                asked = Some(question);
                            }
                            Kind::Info | Kind::Error => ack_due = true,
                        }
                    }
                    (Reply::Success, Some(Req::Start(_))) => started = true,
                    (Reply::Success, _) if conversing => {
                        granted = true;
                        waiting = None;
                    }
                    (_, Some(Req::Cancel)) => waiting = None,
                    (Reply::Refuse(_), _) if conversing => waiting = None,
                    _ => {}
                }
            }
            Event::Ui(shown) => {
                let text = match shown {
                    Shown::Prompt(text, secret) => {
                        field = Some((text.clone(), *secret));
                        text
                    }
                    Shown::Info(text) | Shown::Error(text) => text,
                    Shown::Busy(on) => {
                        busy = *on;
                        continue;
                    }
                    Shown::Reset => continue,
                };
                let from_greetd = heard
                    .iter()
                    .any(|said| said == text || displayed(said) == *text);
                if !from_greetd && let Some(mark) = typed.iter().find(|mark| text.contains(*mark)) {
                    return Err(format!(
                        "typed text ({mark}) is on screen outside its field: {text:?}"
                    ));
                }
            }
            Event::Log(text) => {
                if let Some(mark) = typed.iter().find(|mark| text.contains(*mark)) {
                    return Err(format!("typed text ({mark}) is in the journal:\n{text}"));
                }
                if let Some(line) = text.lines().find(|line| !line.starts_with("vigil: ")) {
                    return Err(format!("a journal line is not vigil's own: {line:?}"));
                }
                let forged = heard
                    .iter()
                    .flat_map(|said| said.split('\n').skip(1))
                    .find(|rest| !rest.is_empty() && text.lines().any(|line| line == *rest));
                if let Some(line) = forged {
                    return Err(format!(
                        "text from greetd wrote its own journal line: {line:?}"
                    ));
                }
                step.log.push_str(text);
            }
            Event::Returned { error, complete } => {
                // The greeter writes a returned error to its journal as it is.
                if let Some(error) = error {
                    if error.chars().any(char::is_control) {
                        return Err(format!("an error carries control characters: {error:?}"));
                    }
                    if let Some(mark) = typed.iter().find(|mark| error.contains(*mark)) {
                        return Err(format!("typed text ({mark}) is in an error: {error}"));
                    }
                }
                if busy {
                    return Err("the step returned with the UI still busy".into());
                }
                if *complete != started {
                    return Err(format!(
                        "is_complete() is {complete} though start_session accepted is {started}"
                    ));
                }
                if !case.conforming {
                    continue;
                }
                let leaves = matches!(step.op, Some(Op::Escape | Op::Pick(_)));
                if leaves && step.waiting.is_some() {
                    if step.requests.first() != Some(&Req::Cancel) {
                        return Err(format!(
                            "{:?} left a waiting conversation without cancel_session",
                            step.op
                        ));
                    }
                    if !step.log.contains("vigil: auth close user=") {
                        return Err(format!(
                            "{:?} closed a waiting conversation without a journal line",
                            step.op
                        ));
                    }
                }
                if let Some(question) = &waiting
                    && field.as_ref() != Some(question)
                {
                    return Err(format!(
                        "greetd waits on {question:?} but the field shows {field:?}"
                    ));
                }
                if ack_due {
                    return Err("a message from greetd was left unacknowledged".into());
                }
                if error.is_some() && !started {
                    return Err(format!(
                        "{:?} failed against a well-behaved greetd",
                        step.op
                    ));
                }
            }
        }
    }
    Ok(())
}

fn question_text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => Just("Password:".to_owned()),
        2 => Just("Password: ".to_owned()),
        1 => Just("Password".to_owned()),
        3 => Just("PIN: ".to_owned()),
        2 => Just("Verification code:".to_owned()),
        2 => Just(format!("Password for {ECHO}:")),
        1 => Just("PIN:\u{7}".to_owned()),
        1 => Just("Username".to_owned()),
        2 => any::<String>(),
        1 => proptest::collection::vec(
            prop_oneof![
                Just('\n'), Just('\r'), Just('\u{1b}'), Just('\0'), Just('\u{7f}'),
                Just('\u{202e}'), Just('"'), Just('\\'), Just(':'), Just(' '), Just('P'),
            ],
            0..24,
        )
        .prop_map(|chars| chars.into_iter().collect()),
        1 => (1usize..200_000).prop_map(|len| "A".repeat(len)),
    ]
}

fn refusal_text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => Just("pam_authenticate: AUTH_ERR".to_owned()),
        1 => Just("pam_authenticate: MAXTRIES".to_owned()),
        1 => Just("pam_acct_mgmt: ACCT_EXPIRED".to_owned()),
        1 => Just("pam_authenticate: ".to_owned()),
        1 => Just(format!("no such user: {ECHO}")),
        1 => Just("refused\nvigil: [t+0ms] auth: start_session accepted".to_owned()),
        1 => any::<String>(),
    ]
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        3 => Just(Kind::Secret),
        2 => Just(Kind::Visible),
        1 => Just(Kind::Info),
        1 => Just(Kind::Error),
    ]
}

fn frame() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..64),
        Just(br#"{"type":"success","extra":1}"#.to_vec()),
        Just(br#"{"type":"auth_message"}"#.to_vec()),
        Just(br#"{"type":"auth_message","auth_message_type":"otp","auth_message":"x"}"#.to_vec()),
        Just(
            b"{\"type\":\"error\",\"error_type\":\"error\",\"description\":\"\xff\xfe\"}".to_vec()
        ),
        Just(b"[]".to_vec()),
        Just(Vec::new()),
    ]
}

fn reply(conforming: bool) -> BoxedStrategy<Reply> {
    let spoken = prop_oneof![
        6 => (kind(), question_text()).prop_map(|(kind, text)| Reply::Message(kind, text)),
        3 => Just(Reply::Success),
        2 => refusal_text().prop_map(Reply::Refuse),
    ];
    if conforming {
        spoken.boxed()
    } else {
        let oversized = vigil_auth::MAX_RESPONSE_BYTES as u32 + 1..=u32::MAX;
        prop_oneof![
            10 => spoken,
            2 => frame().prop_map(Reply::Frame),
            1 => oversized.prop_map(Reply::Claim),
        ]
        .boxed()
    }
}

fn typed() -> impl Strategy<Value = String> {
    let edge = prop_oneof![
        3 => Just(String::new()),
        2 => "[ -~]{0,12}",
        1 => any::<String>(),
        1 => (1usize..100_000).prop_map(|len| "x".repeat(len)),
    ];
    (edge.clone(), 0u16..10_000, edge)
        .prop_map(|(before, id, after)| format!("{before}{MARK}{id:04}{after}"))
}

fn user() -> impl Strategy<Value = Option<usize>> {
    proptest::option::weighted(0.8, 0..USERS.len())
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => typed().prop_map(Op::Type),
        2 => Just(Op::Enter),
        1 => Just(Op::TypeDefaultName),
        3 => Just(Op::Escape),
        3 => user().prop_map(Op::Pick),
        1 => "[a-z]{1,8}".prop_map(|name| Op::Session(vec![name])),
    ]
}

fn case(conforming: bool) -> impl Strategy<Value = Case> {
    let first = prop_oneof![user().prop_map(Op::Begin), user().prop_map(Op::Pick)];
    (
        proptest::option::of(0..USERS.len()),
        proptest::option::weighted(0.3, 0..USERS.len()),
        proptest::collection::vec((0..USERS.len(), question_text(), any::<bool>()), 0..3),
        first,
        proptest::collection::vec(op(), 0..24),
        proptest::collection::vec(reply(conforming), 0..24),
        proptest::collection::vec(any::<bool>(), 0..3),
        proptest::bool::weighted(0.1),
    )
        .prop_map(
            move |(
                default_user,
                unprompted,
                remembered,
                first,
                mut ops,
                replies,
                starts,
                refuse_cancels,
            )| {
                ops.insert(0, first);
                Case {
                    conforming,
                    default_user,
                    unprompted,
                    remembered,
                    ops,
                    replies,
                    starts,
                    refuse_cancels,
                }
            },
        )
}

fn attack(greetd: &Greetd, name: &str, conforming: bool, cases: u32) -> bool {
    let started = Instant::now();
    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    });
    let outcome = runner.run(&case(conforming), |case| {
        let trace = drive(greetd, &case);
        check(&case, &trace).map_err(|why| TestCaseError::fail(format!("{why}\n{trace:#?}")))
    });
    match outcome {
        Ok(()) => {
            println!(
                "attack: {name}: {cases} cases held in {:.1?}",
                started.elapsed()
            );
            true
        }
        Err(failure) => {
            println!("attack: {name}: FAILED\n{failure}");
            false
        }
    }
}

fn pinned() -> Vec<(&'static str, Case)> {
    let refused = |name: &'static str, ops: Vec<Op>, description: &str| {
        let case = Case {
            conforming: true,
            default_user: None,
            unprompted: None,
            remembered: Vec::new(),
            ops,
            replies: vec![Reply::Refuse(description.into())],
            starts: Vec::new(),
            refuse_cancels: false,
        };
        (name, case)
    };
    vec![
        refused(
            "greetd's error repeats a typed name",
            vec![
                Op::Begin(None),
                Op::Type(format!("{MARK}0001")),
                Op::Type(format!("{MARK}0002")),
            ],
            &format!("no such user: {ECHO}"),
        ),
        refused(
            "greetd's error carries a line of its own",
            vec![Op::Begin(Some(0)), Op::Enter],
            "refused\nvigil: [t+0ms] auth: start_session accepted",
        ),
        (
            "a failed unasked open leaves the previous user's field on screen",
            Case {
                conforming: false,
                default_user: Some(0),
                unprompted: Some(1),
                remembered: vec![(1, "Password:".into(), false)],
                ops: vec![
                    Op::Begin(Some(0)),
                    Op::Pick(Some(1)),
                    Op::Type(format!("{MARK}0000")),
                ],
                replies: vec![
                    Reply::Frame(Vec::new()),
                    Reply::Message(Kind::Visible, "Password:".into()),
                ],
                starts: Vec::new(),
                refuse_cancels: false,
            },
        ),
    ]
}

fn replay(greetd: &Greetd) -> bool {
    let mut held = true;
    for (name, case) in pinned() {
        match check(&case, &drive(greetd, &case)) {
            Ok(()) => println!("attack: pinned: {name}: held"),
            Err(why) => {
                println!("attack: pinned: {name}: FAILED\n{why}");
                held = false;
            }
        }
    }
    held
}

fn main() {
    let greetd = match Greetd::bind() {
        Ok(greetd) => greetd,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("attack: SKIP, unix sockets are not permitted here ({error})");
            std::process::exit(i32::from(std::env::var_os("CI").is_some()));
        }
        Err(error) => panic!("bind the scripted greetd: {error}"),
    };
    let cases = std::env::var("VIGIL_ATTACK_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(1500);
    let held = replay(&greetd)
        & attack(&greetd, "a greetd that follows the protocol", true, cases)
        & attack(&greetd, "a greetd that answers anything", false, cases);
    drop(greetd);
    std::process::exit(i32::from(!held));
}
