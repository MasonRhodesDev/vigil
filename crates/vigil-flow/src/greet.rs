//! Pure greeter stage controller (issue #61).
//!
//! The greeter's login flow — username stage, account and session
//! selection, the greetd conversation, session start — with the same
//! contract as [`crate::LockFlow`]: events in, commands out, no I/O.
//!
//! `vigil-auth::AuthMachine` previously *was* this machine, but it performed
//! the greetd socket round-trip synchronously inside each transition
//! (`transact`) and pushed straight into a `&mut dyn AuthUi`. That coupling
//! is what made the greeter impossible to drive with a fake clock or to
//! preview in the simulator. Here every round-trip becomes a request
//! command out and a [`GreetEvent::GreetdReply`] back in.
//!
//! Because greetd is strictly request/response, a request that must be
//! followed by another (cancel-then-reopen) is modelled as a *pending
//! continuation* rather than two commands in one batch — emitting both at
//! once desynchronises the reply stream, and the second reply then answers
//! the wrong question.
//!
//! This is deliberately *not* merged with [`crate::LockFlow`]: the greeter
//! authenticates through greetd and ends by execing a session, the locker
//! authenticates through PAM directly and ends by releasing a lock. They
//! share this crate, not a machine.

use std::time::Duration;

use vigil_core::{PowerAction, UiMessage};

use crate::Now;

/// NOTE this presupposes the adapter treats the greetd socket as an event
/// source. `AuthMachine::transact` is a blocking round-trip today; an
/// adapter that keeps it synchronous and synthesises a reply immediately
/// after each command renders this deadline, [`GreetEvent::Tick`] and
/// [`GreetFlow::next_wake`] inert, and the hang this prevents comes back.
///
/// How long a greetd request may go unanswered before the greeter stops
/// waiting. `AuthMachine` blocked on the socket, so a hung greetd froze the
/// process visibly; making the round-trip asynchronous turns that into a
/// live event loop attached to a permanently deaf machine — the spinner
/// latched on and every keypress ignored. This is the deadline that keeps
/// the greeter answerable.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Prompt text for the greeter-owned username stage.
pub const USERNAME_PROMPT: &str = "Username";

/// What greetd said, decoded from its IPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreetdReply {
    /// A prompt the user must answer.
    Prompt { text: String, secret: bool },
    /// Conversational text; the protocol expects an empty response so the
    /// conversation continues.
    Info(String),
    /// Conversational error text; likewise continues the conversation.
    Notice(String),
    /// The outstanding request succeeded.
    Success,
    /// The outstanding request failed. `auth` distinguishes greetd's
    /// `ErrorType::AuthError` (wrong password — reopen and let the user
    /// retry) from `ErrorType::Error` (the request itself was refused —
    /// reopening just loops).
    Error { auth: bool, description: String },
}

#[derive(Clone, PartialEq)]
pub enum GreetEvent {
    /// A message from the UI (respond, cancel, select, power).
    Ui(UiMessage),
    /// greetd answered the request that was outstanding.
    GreetdReply(GreetdReply),
    /// The connection to greetd failed irrecoverably.
    GreetdLost(String),
    /// Timer wakeup; the only thing that can fire the request deadline.
    Tick,
}

#[derive(Clone, PartialEq)]
pub enum GreetCmd {
    ShowPrompt {
        text: String,
        secret: bool,
    },
    ShowInfo(String),
    ShowError(String),
    SetBusy(bool),
    /// Ask greetd to open a session for this user.
    CreateSession(String),
    /// Answer the outstanding auth message. `None` is the empty response an
    /// info/error message requires.
    PostResponse(Option<String>),
    /// Authentication finished; launch the selected session.
    StartSession {
        cmd: Vec<String>,
        env: Vec<String>,
    },
    /// Abandon the in-flight conversation.
    CancelSession,
    /// The greeter's own selection state changed. `None` is the manual-entry
    /// row, which clears the remembered account so the field is typed into.
    SelectedUser {
        index: usize,
        user: Option<String>,
    },
    SelectedSession(usize),
    Power(PowerAction),
    /// Terminal: the greeter is done, and how it ended. Mirrors
    /// `FlowCmd::Exit` so an adapter has one place to ask "am I finished".
    Exit(GreetOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreetOutcome {
    /// The session is running; the greeter should exit.
    Started,
    /// Unrecoverable: the greeter cannot continue.
    Fatal(String),
}

/// Hand-written so a typed password cannot reach a log. `vigil-sim`
/// already traces `{event:?}` and vigil-lock has ~40 `{:?}` sites, so a
/// derived Debug here is one debugging `eprintln!` away from putting
/// passwords in the journal — and the tests themselves print command
/// vectors in assertion messages.
impl std::fmt::Debug for GreetEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ui(UiMessage::Respond(_)) => f.write_str("Ui(Respond(<redacted>))"),
            Self::Ui(message) => write!(f, "Ui({message:?})"),
            Self::GreetdReply(reply) => write!(f, "GreetdReply({reply:?})"),
            Self::Tick => f.write_str("Tick"),
            Self::GreetdLost(error) => write!(f, "GreetdLost({error:?})"),
        }
    }
}

impl std::fmt::Debug for GreetCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PostResponse(Some(_)) => f.write_str("PostResponse(<redacted>)"),
            Self::PostResponse(None) => f.write_str("PostResponse(None)"),
            Self::ShowPrompt { text, secret } => {
                write!(f, "ShowPrompt {{ text: {text:?}, secret: {secret} }}")
            }
            Self::ShowInfo(text) => write!(f, "ShowInfo({text:?})"),
            Self::ShowError(text) => write!(f, "ShowError({text:?})"),
            Self::SetBusy(on) => write!(f, "SetBusy({on})"),
            Self::CreateSession(user) => write!(f, "CreateSession({user:?})"),
            Self::StartSession { cmd, env } => {
                write!(f, "StartSession {{ cmd: {cmd:?}, env: {env:?} }}")
            }
            Self::CancelSession => f.write_str("CancelSession"),
            Self::SelectedUser { index, user } => {
                write!(f, "SelectedUser {{ index: {index}, user: {user:?} }}")
            }
            Self::SelectedSession(index) => write!(f, "SelectedSession({index})"),
            Self::Power(action) => write!(f, "Power({action:?})"),
            Self::Exit(outcome) => write!(f, "Exit({outcome:?})"),
        }
    }
}

/// Which request is awaiting a reply, and what to do once it lands. greetd
/// is strictly request/response, so the same `Success` means different
/// things depending on what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    None,
    /// A create_session or a posted response — greetd answers both the same
    /// way, and no arm ever discriminated them.
    Auth,
    Start,
    /// A cancel whose reply is uninteresting; then reopen for this account,
    /// or return to the username stage when there is none.
    CancelThen(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreetPhase {
    /// Nothing in flight; greetd has not been contacted.
    Idle,
    /// The greeter itself is asking who is logging in.
    AwaitingUser,
    /// A greetd auth message is on screen awaiting an answer.
    AwaitingPrompt,
    /// A request is in flight.
    Busy,
    /// The session is running.
    Complete,
}

/// One session the picker can launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionChoice {
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

/// Everything the greeter knows before the first event.
#[derive(Debug, Clone, Default)]
pub struct GreetConfig {
    /// Account fixed by configuration (kiosk/autologin). The picker is inert
    /// and the username stage is skipped.
    pub fixed_user: Option<String>,
    /// Picker rows. `None` is the manual-entry row ("Other…"): the flow
    /// never sees that label, only its meaning.
    pub users: Vec<Option<String>>,
    pub selected_user: usize,
    pub sessions: Vec<SessionChoice>,
    pub selected_session: usize,
}

pub struct GreetFlow {
    phase: GreetPhase,
    pending: Pending,
    user: Option<String>,
    fixed_user: bool,
    /// The account an empty submission uses; changed by the picker.
    default_user: Option<String>,
    config: GreetConfig,
    session: SessionChoice,
    /// An auth failure is re-raised *after* the next prompt, or the user
    /// never sees why they are retyping — a fresh prompt clears messages.
    pending_error: Option<String>,
    /// Deadline for the outstanding request, on the injected clock.
    deadline: Option<Duration>,
    /// Time until that deadline as of the last step, so `next_wake` reads
    /// like [`crate::LockFlow::next_wake`] and adapters see one contract.
    wait: Option<Duration>,
}

impl GreetFlow {
    pub fn new(now: Now, config: GreetConfig) -> (Self, Vec<GreetCmd>) {
        let fixed = config.fixed_user.clone();
        let default_user = config.users.get(config.selected_user).cloned().flatten();
        let session = config
            .sessions
            .get(config.selected_session)
            .cloned()
            .unwrap_or_default();
        let mut flow = Self {
            phase: GreetPhase::Idle,
            pending: Pending::None,
            user: None,
            fixed_user: fixed.is_some(),
            default_user,
            config,
            session,
            pending_error: None,
            deadline: None,
            wait: None,
        };
        let mut cmds = Vec::new();
        match fixed {
            Some(user) => flow.create_session(now, user, &mut cmds),
            None => flow.prompt_username(&mut cmds),
        }
        // Prime `wait`, as LockFlow::new does: the autologin path arms a
        // deadline here, and an adapter that sleeps until next_wake() would
        // otherwise arm no timer, never send a Tick, and never recompute —
        // wedging on the very hang REQUEST_TIMEOUT exists to prevent.
        flow.wait = flow
            .deadline
            .map(|deadline| deadline.saturating_sub(now.elapsed));
        (flow, cmds)
    }

    pub fn phase(&self) -> GreetPhase {
        self.phase
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The session `StartSession` would launch right now.
    pub fn session(&self) -> &SessionChoice {
        &self.session
    }

    /// Time until the outstanding request must be answered, as of the last
    /// [`Self::step`], or `None` when nothing is in flight. The consumer
    /// arms a timer on this and feeds [`GreetEvent::Tick`]; without it a
    /// silent greetd is unrecoverable.
    pub fn next_wake(&self) -> Option<Duration> {
        self.wait
    }

    pub fn step(&mut self, now: Now, event: GreetEvent) -> Vec<GreetCmd> {
        let mut cmds = Vec::new();
        if self.phase == GreetPhase::Complete {
            // The session is running; nothing may reopen a conversation.
            return cmds;
        }
        match event {
            GreetEvent::Ui(message) => self.on_ui(now, message, &mut cmds),
            GreetEvent::GreetdReply(reply) => {
                self.deadline = None;
                self.on_reply(now, reply, &mut cmds);
            }
            GreetEvent::GreetdLost(error) => {
                self.pending = Pending::None;
                self.deadline = None;
                cmds.push(GreetCmd::SetBusy(false));
                cmds.push(GreetCmd::Exit(GreetOutcome::Fatal(error)));
                self.phase = GreetPhase::Complete;
            }
            GreetEvent::Tick => {}
        }
        // Any step is a chance to notice the deadline.
        if let Some(deadline) = self.deadline
            && now.elapsed >= deadline
        {
            self.deadline = None;
            self.pending = Pending::None;
            cmds.push(GreetCmd::SetBusy(false));
            cmds.push(GreetCmd::ShowError("greetd did not respond".into()));
            self.recover(now, &mut cmds);
        }
        self.wait = self
            .deadline
            .map(|deadline| deadline.saturating_sub(now.elapsed));
        cmds
    }

    fn on_ui(&mut self, now: Now, message: UiMessage, cmds: &mut Vec<GreetCmd>) {
        match message {
            UiMessage::Respond(text) => match self.phase {
                GreetPhase::AwaitingUser => {
                    let text = text.trim().to_owned();
                    // An empty submission takes the remembered account, so
                    // the common case is one keypress.
                    let user = if text.is_empty() {
                        self.default_user.clone()
                    } else {
                        Some(text)
                    };
                    match user {
                        Some(user) => self.create_session(now, user, cmds),
                        None => self.prompt_username(cmds),
                    }
                }
                GreetPhase::AwaitingPrompt => {
                    self.pending = Pending::Auth;
                    self.phase = GreetPhase::Busy;
                    self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
                    cmds.push(GreetCmd::SetBusy(true));
                    cmds.push(GreetCmd::PostResponse(Some(text)));
                }
                // A submission with nothing outstanding is a stray repeat.
                _ => {}
            },
            // Cancel is also the escape hatch out of Idle: a failed
            // start_session must not leave the greeter with no way forward.
            UiMessage::Cancel => match self.phase {
                GreetPhase::AwaitingPrompt => self.cancel_then(now, None, cmds),
                GreetPhase::Idle => self.recover(now, cmds),
                _ => {}
            },
            UiMessage::SelectUser(index) => {
                if self.fixed_user {
                    // Configuration pinned the account; the picker is inert.
                    return;
                }
                let Some(user) = self.config.users.get(index).cloned() else {
                    return;
                };
                self.config.selected_user = index;
                self.default_user = user.clone();
                cmds.push(GreetCmd::SelectedUser {
                    index,
                    user: user.clone(),
                });
                // Switching accounts abandons the old conversation; starting
                // the new one waits for the cancel's reply.
                match self.phase {
                    GreetPhase::AwaitingPrompt | GreetPhase::Busy => {
                        self.cancel_then(now, user, cmds)
                    }
                    _ => match user {
                        Some(user) => self.create_session(now, user, cmds),
                        None => self.prompt_username(cmds),
                    },
                }
            }
            UiMessage::SelectSession(index) => {
                let Some(session) = self.config.sessions.get(index).cloned() else {
                    return;
                };
                self.config.selected_session = index;
                self.session = session;
                cmds.push(GreetCmd::SelectedSession(index));
            }
            UiMessage::Power(action) => cmds.push(GreetCmd::Power(action)),
        }
    }

    fn on_reply(&mut self, now: Now, reply: GreetdReply, cmds: &mut Vec<GreetCmd>) {
        let pending = std::mem::replace(&mut self.pending, Pending::None);
        match pending {
            // A reply with nothing outstanding is a protocol desync; showing
            // a prompt for it would wire the UI to a stream we cannot answer.
            Pending::None => {}
            // The cancel's own reply is uninteresting whatever it says.
            Pending::CancelThen(user) => match user {
                Some(user) => self.create_session(now, user, cmds),
                None => self.recover(now, cmds),
            },
            // Past start_session there is no conversation left; an auth
            // message here is a protocol violation, not a prompt.
            Pending::Start => match reply {
                GreetdReply::Success => {
                    self.phase = GreetPhase::Complete;
                    cmds.push(GreetCmd::SetBusy(false));
                    cmds.push(GreetCmd::Exit(GreetOutcome::Started));
                }
                GreetdReply::Error { description, .. } => {
                    cmds.push(GreetCmd::SetBusy(false));
                    cmds.push(GreetCmd::ShowError(description));
                    // Never a dead end: the session command may simply be
                    // gone after an update, and the user typed the right
                    // password.
                    self.recover(now, cmds);
                }
                _ => {
                    cmds.push(GreetCmd::SetBusy(false));
                    cmds.push(GreetCmd::Exit(GreetOutcome::Fatal(
                        "greetd returned an authentication message after start_session".into(),
                    )));
                    self.phase = GreetPhase::Complete;
                }
            },
            Pending::Auth => match reply {
                GreetdReply::Prompt { text, secret } => {
                    cmds.push(GreetCmd::SetBusy(false));
                    cmds.push(GreetCmd::ShowPrompt { text, secret });
                    if let Some(error) = self.pending_error.take() {
                        cmds.push(GreetCmd::ShowError(error));
                    }
                    self.phase = GreetPhase::AwaitingPrompt;
                }
                GreetdReply::Info(text) => {
                    cmds.push(GreetCmd::ShowInfo(text));
                    self.pending = Pending::Auth;
                    self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
                    cmds.push(GreetCmd::PostResponse(None));
                }
                GreetdReply::Notice(text) => {
                    cmds.push(GreetCmd::ShowError(text));
                    self.pending = Pending::Auth;
                    self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
                    cmds.push(GreetCmd::PostResponse(None));
                }
                GreetdReply::Success => {
                    self.pending = Pending::Start;
                    self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
                    cmds.push(GreetCmd::StartSession {
                        cmd: self.session.cmd.clone(),
                        env: self.session.env.clone(),
                    });
                }
                GreetdReply::Error { auth, description } => {
                    // Shown now *and* re-raised after the next prompt: the
                    // reopen spans two round-trips, so without the immediate
                    // one a slow greetd leaves a cleared field and no reason.
                    cmds.push(GreetCmd::ShowError(description.clone()));
                    if !auth {
                        // greetd refused the request itself; reopening the
                        // same conversation just loops.
                        self.recover(now, cmds);
                        return;
                    }
                    self.pending_error = Some(description);
                    let user = self.user.clone();
                    self.cancel_then(now, user, cmds);
                }
            },
        }
    }

    /// Abandon the in-flight conversation, then continue with `user` (or the
    /// username stage). The continuation waits for the cancel's reply —
    /// sending both requests at once desynchronises the reply stream.
    fn cancel_then(&mut self, now: Now, user: Option<String>, cmds: &mut Vec<GreetCmd>) {
        // A fixed account has no username stage to fall back to.
        let user = user.or_else(|| self.fixed_user.then(|| self.user.clone()).flatten());
        cmds.push(GreetCmd::CancelSession);
        self.pending = Pending::CancelThen(user);
        self.phase = GreetPhase::Busy;
        self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
    }

    /// Somewhere to go from Idle: the username stage, or a fresh
    /// conversation when the account is fixed.
    fn recover(&mut self, now: Now, cmds: &mut Vec<GreetCmd>) {
        match (self.fixed_user, self.user.clone()) {
            (true, Some(user)) => self.create_session(now, user, cmds),
            _ => self.prompt_username(cmds),
        }
    }

    fn create_session(&mut self, now: Now, user: String, cmds: &mut Vec<GreetCmd>) {
        self.user = Some(user.clone());
        self.pending = Pending::Auth;
        self.phase = GreetPhase::Busy;
        self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
        cmds.push(GreetCmd::SetBusy(true));
        cmds.push(GreetCmd::CreateSession(user));
    }

    fn prompt_username(&mut self, cmds: &mut Vec<GreetCmd>) {
        self.user = None;
        self.pending = Pending::None;
        self.phase = GreetPhase::AwaitingUser;
        cmds.push(GreetCmd::SetBusy(false));
        cmds.push(GreetCmd::ShowPrompt {
            text: USERNAME_PROMPT.into(),
            secret: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hyprland() -> SessionChoice {
        SessionChoice {
            cmd: vec!["Hyprland".into()],
            env: vec!["XDG_SESSION_TYPE=wayland".into()],
        }
    }

    fn sway() -> SessionChoice {
        SessionChoice {
            cmd: vec!["sway".into()],
            env: vec![],
        }
    }

    /// Two picker rows plus the manual-entry row, two sessions.
    fn config() -> GreetConfig {
        GreetConfig {
            fixed_user: None,
            users: vec![Some("mason".into()), Some("guest".into()), None],
            selected_user: 0,
            sessions: vec![hyprland(), sway()],
            selected_session: 0,
        }
    }

    fn kiosk() -> GreetConfig {
        GreetConfig {
            fixed_user: Some("kiosk".into()),
            ..config()
        }
    }

    fn has(cmds: &[GreetCmd], wanted: &GreetCmd) -> bool {
        cmds.contains(wanted)
    }

    fn at(ms: u64) -> Now {
        Now::at(Duration::from_millis(ms))
    }

    fn err(auth: bool, description: &str) -> GreetEvent {
        GreetEvent::GreetdReply(GreetdReply::Error {
            auth,
            description: description.into(),
        })
    }

    fn prompt(text: &str, secret: bool) -> GreetEvent {
        GreetEvent::GreetdReply(GreetdReply::Prompt {
            text: text.into(),
            secret,
        })
    }

    fn respond(text: &str) -> GreetEvent {
        GreetEvent::Ui(UiMessage::Respond(text.into()))
    }

    fn ok() -> GreetEvent {
        GreetEvent::GreetdReply(GreetdReply::Success)
    }

    /// Drive a flow to a live password prompt. Without a fixed account the
    /// username stage comes first, so submit one.
    fn at_prompt(config: GreetConfig) -> GreetFlow {
        let fixed = config.fixed_user.is_some();
        let (mut flow, _) = GreetFlow::new(at(0), config);
        if !fixed {
            flow.step(at(0), respond("mason"));
        }
        flow.step(at(0), prompt("Password:", true));
        assert_eq!(flow.phase(), GreetPhase::AwaitingPrompt);
        flow
    }

    #[test]
    fn the_username_stage_precedes_greetd() {
        let (mut flow, boot) = GreetFlow::new(
            at(0),
            GreetConfig {
                users: vec![None],
                ..config()
            },
        );
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
        assert!(has(
            &boot,
            &GreetCmd::ShowPrompt {
                text: USERNAME_PROMPT.into(),
                secret: false
            }
        ));
        assert!(!boot.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))));
        let cmds = flow.step(at(0), respond("mason"));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
        assert_eq!(flow.user(), Some("mason"));
    }

    #[test]
    fn a_fixed_user_skips_the_username_stage_and_pins_the_picker() {
        let (mut flow, boot) = GreetFlow::new(at(0), kiosk());
        assert!(has(&boot, &GreetCmd::CreateSession("kiosk".into())));
        // The account is configuration, not a choice.
        assert!(
            flow.step(at(0), GreetEvent::Ui(UiMessage::SelectUser(1)))
                .is_empty()
        );
        assert_eq!(flow.user(), Some("kiosk"));
    }

    #[test]
    fn an_empty_submission_takes_the_selected_account() {
        let (mut flow, _) = GreetFlow::new(
            at(0),
            GreetConfig {
                selected_user: 0,
                ..config()
            },
        );
        // selected_user 0 is "mason"; the username stage is reached because
        // no account is fixed.
        flow.step(at(0), GreetEvent::Ui(UiMessage::SelectUser(2)));
        let (mut flow2, _) = GreetFlow::new(at(0), config());
        let cmds = flow2.step(at(0), respond("   "));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
        // The manual-entry row clears it, so the field is typed into.
        let cmds = flow.step(at(0), respond(""));
        assert!(!cmds.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))));
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
    }

    #[test]
    fn a_password_prompt_is_answered_and_starts_the_session() {
        let mut flow = at_prompt(kiosk());
        let cmds = flow.step(at(0), respond("hunter2"));
        assert!(has(&cmds, &GreetCmd::PostResponse(Some("hunter2".into()))));
        let cmds = flow.step(at(0), ok());
        assert!(has(
            &cmds,
            &GreetCmd::StartSession {
                cmd: vec!["Hyprland".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            }
        ));
        let cmds = flow.step(at(0), ok());
        assert!(has(&cmds, &GreetCmd::Exit(GreetOutcome::Started)));
        assert_eq!(flow.phase(), GreetPhase::Complete);
    }

    #[test]
    fn cancel_waits_for_its_own_reply_before_reopening() {
        // greetd is request/response: emitting cancel AND create in one
        // batch desynchronises the stream, and the cancel's ack is then
        // read as authentication success — a session start with no password.
        let mut flow = at_prompt(kiosk());
        let cmds = flow.step(at(0), GreetEvent::Ui(UiMessage::Cancel));
        assert!(has(&cmds, &GreetCmd::CancelSession));
        assert!(
            !cmds.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))),
            "the reopen must wait for the cancel's reply: {cmds:?}"
        );
        // The cancel's ack must never be mistaken for auth success.
        let cmds = flow.step(at(0), ok());
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, GreetCmd::StartSession { .. })),
            "cancel ack started a session without authentication: {cmds:?}"
        );
        assert!(has(&cmds, &GreetCmd::CreateSession("kiosk".into())));
    }

    #[test]
    fn cancel_returns_a_switchable_greeter_to_the_username_stage() {
        let mut flow = at_prompt(config());
        flow.step(at(0), GreetEvent::Ui(UiMessage::Cancel));
        let cmds = flow.step(at(0), ok());
        assert!(has(
            &cmds,
            &GreetCmd::ShowPrompt {
                text: USERNAME_PROMPT.into(),
                secret: false
            }
        ));
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
    }

    #[test]
    fn a_failed_start_session_leaves_a_way_forward() {
        // greetd rejects start_session whenever the session command cannot
        // run — a .desktop whose Exec vanished in an update. The user typed
        // the right password; parking with a dead field locks them out.
        let mut flow = at_prompt(config());
        flow.step(at(0), respond("ok"));
        flow.step(at(0), ok());
        let cmds = flow.step(at(0), err(false, "no such session"));
        assert!(has(&cmds, &GreetCmd::ShowError("no such session".into())));
        assert!(!has(&cmds, &GreetCmd::Exit(GreetOutcome::Started)));
        // Recovery is immediate rather than parked behind a Cancel the user
        // has no reason to press: they typed the right password and the
        // session command was simply missing.
        assert!(
            cmds.iter()
                .any(|c| matches!(c, GreetCmd::ShowPrompt { .. })),
            "no way forward after a failed start: {cmds:?}"
        );
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
    }

    #[test]
    fn switching_account_abandons_the_old_conversation() {
        let mut flow = at_prompt(config());
        let cmds = flow.step(at(0), GreetEvent::Ui(UiMessage::SelectUser(1)));
        assert!(has(
            &cmds,
            &GreetCmd::SelectedUser {
                index: 1,
                user: Some("guest".into())
            }
        ));
        assert!(has(&cmds, &GreetCmd::CancelSession));
        // ...and only opens the new one once the cancel is answered.
        assert!(!cmds.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))));
        let cmds = flow.step(at(0), ok());
        assert!(has(&cmds, &GreetCmd::CreateSession("guest".into())));
        assert_eq!(flow.user(), Some("guest"));
    }

    #[test]
    fn the_manual_entry_row_returns_to_the_username_stage() {
        let mut flow = at_prompt(config());
        let cmds = flow.step(at(0), GreetEvent::Ui(UiMessage::SelectUser(2)));
        assert!(has(
            &cmds,
            &GreetCmd::SelectedUser {
                index: 2,
                user: None
            }
        ));
        let cmds = flow.step(at(0), ok());
        assert!(
            cmds.iter()
                .any(|c| matches!(c, GreetCmd::ShowPrompt { .. }))
        );
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
    }

    #[test]
    fn the_session_picker_changes_what_is_launched() {
        let mut flow = at_prompt(kiosk());
        let cmds = flow.step(at(0), GreetEvent::Ui(UiMessage::SelectSession(1)));
        assert!(has(&cmds, &GreetCmd::SelectedSession(1)));
        assert_eq!(flow.session(), &sway());
        flow.step(at(0), respond("ok"));
        let cmds = flow.step(at(0), ok());
        assert!(
            has(
                &cmds,
                &GreetCmd::StartSession {
                    cmd: vec!["sway".into()],
                    env: vec![],
                }
            ),
            "the picked session must be the one launched: {cmds:?}"
        );
    }

    #[test]
    fn an_auth_failure_is_shown_now_and_again_after_the_new_prompt() {
        // A fresh prompt clears UI messages, so the reason must be repeated
        // after it — but the reopen spans two round-trips, so it must also
        // appear immediately or a slow greetd shows a cleared field and
        // nothing else.
        let mut flow = at_prompt(kiosk());
        flow.step(at(0), respond("wrong"));
        let cmds = flow.step(at(0), err(true, "authentication failed"));
        assert!(has(
            &cmds,
            &GreetCmd::ShowError("authentication failed".into())
        ));
        assert!(has(&cmds, &GreetCmd::CancelSession));

        let cmds = flow.step(at(0), ok());
        assert!(has(&cmds, &GreetCmd::CreateSession("kiosk".into())));
        let cmds = flow.step(at(0), prompt("Password:", true));
        let prompt_at = cmds
            .iter()
            .position(|c| matches!(c, GreetCmd::ShowPrompt { .. }));
        let error_at = cmds
            .iter()
            .position(|c| matches!(c, GreetCmd::ShowError(_)));
        assert!(prompt_at.is_some() && error_at.is_some(), "{cmds:?}");
        assert!(
            prompt_at < error_at,
            "error must follow the prompt: {cmds:?}"
        );
    }

    #[test]
    fn info_and_notice_keep_the_conversation_moving() {
        let (mut flow, _) = GreetFlow::new(at(0), kiosk());
        let cmds = flow.step(
            at(0),
            GreetEvent::GreetdReply(GreetdReply::Info("hello".into())),
        );
        assert!(has(&cmds, &GreetCmd::ShowInfo("hello".into())));
        assert!(has(&cmds, &GreetCmd::PostResponse(None)));
        let cmds = flow.step(
            at(0),
            GreetEvent::GreetdReply(GreetdReply::Notice("beware".into())),
        );
        assert!(has(&cmds, &GreetCmd::ShowError("beware".into())));
        assert!(has(&cmds, &GreetCmd::PostResponse(None)));
    }

    #[test]
    fn an_auth_message_after_start_session_is_a_protocol_violation() {
        // Past start_session there is no conversation left. Re-prompting
        // here would wire the UI to a desynchronised socket.
        let mut flow = at_prompt(kiosk());
        flow.step(at(0), respond("ok"));
        flow.step(at(0), ok());
        let cmds = flow.step(at(0), prompt("Again:", true));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, GreetCmd::Exit(GreetOutcome::Fatal(_)))),
            "{cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, GreetCmd::ShowPrompt { .. }))
        );
    }

    #[test]
    fn power_actions_pass_through_at_any_stage() {
        let (mut flow, _) = GreetFlow::new(at(0), config());
        let cmds = flow.step(
            at(0),
            GreetEvent::Ui(UiMessage::Power(PowerAction::Poweroff)),
        );
        assert!(has(&cmds, &GreetCmd::Power(PowerAction::Poweroff)));
    }

    #[test]
    fn a_started_session_ignores_everything_after() {
        let mut flow = at_prompt(kiosk());
        flow.step(at(0), respond("ok"));
        flow.step(at(0), ok());
        flow.step(at(0), ok());
        assert_eq!(flow.phase(), GreetPhase::Complete);
        assert!(flow.step(at(0), respond("again")).is_empty());
        assert!(
            flow.step(at(0), GreetEvent::Ui(UiMessage::Cancel))
                .is_empty()
        );
        assert!(flow.step(at(0), ok()).is_empty());
    }

    #[test]
    fn a_lost_connection_is_terminal() {
        // Without a socket there is nothing the greeter can do. Exit is the
        // one place a consumer has to look, and later events are inert
        // rather than dropped into a machine that cannot act on them.
        let (mut flow, _) = GreetFlow::new(at(0), config());
        let cmds = flow.step(at(0), GreetEvent::GreetdLost("broken pipe".into()));
        assert!(has(&cmds, &GreetCmd::SetBusy(false)));
        assert!(has(
            &cmds,
            &GreetCmd::Exit(GreetOutcome::Fatal("broken pipe".into()))
        ));
        assert_eq!(flow.phase(), GreetPhase::Complete);
        assert!(flow.step(at(0), respond("mason")).is_empty());
    }

    #[test]
    fn a_reply_with_nothing_outstanding_is_ignored() {
        let (mut flow, _) = GreetFlow::new(at(0), config());
        // At the username stage greetd has not been contacted.
        assert!(flow.step(at(0), ok()).is_empty());
    }

    #[test]
    fn construction_arms_a_wake_for_an_autologin_request() {
        // The fixed-user path issues create_session from new(), so a
        // consumer that arms its timer from next_wake() must get one
        // before the first step — otherwise no Tick ever arrives.
        let (flow, _) = GreetFlow::new(at(0), kiosk());
        assert!(
            flow.next_wake().is_some(),
            "no deadline armed at construction"
        );
        // The username stage contacts nobody, so nothing to wait for.
        let (flow, _) = GreetFlow::new(at(0), config());
        assert!(flow.next_wake().is_none());
    }

    #[test]
    fn a_silent_greetd_does_not_wedge_the_greeter() {
        // AuthMachine blocked on the socket, so a hung greetd froze the
        // process visibly. Asynchronous round-trips turn that into a live
        // event loop attached to a deaf machine — spinner latched, every
        // keypress ignored — unless a deadline says otherwise.
        let mut flow = at_prompt(kiosk());
        flow.step(at(0), respond("hunter2"));
        assert!(flow.next_wake().is_some(), "no deadline armed");
        // Nothing arrives.
        let cmds = flow.step(at(REQUEST_TIMEOUT.as_millis() as u64), GreetEvent::Tick);
        assert!(has(&cmds, &GreetCmd::SetBusy(false)), "{cmds:?}");
        assert!(has(
            &cmds,
            &GreetCmd::ShowError("greetd did not respond".into())
        ));
        assert!(
            cmds.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))),
            "the greeter must be answerable again: {cmds:?}"
        );
        // The reopened conversation arms its own deadline, so a greetd that
        // stays silent cannot wedge the retry either.
        assert!(flow.next_wake().is_some());
    }

    #[test]
    fn a_non_auth_error_does_not_loop_forever() {
        // greetd's ErrorType distinguishes a wrong password (retry) from a
        // refused request (reopening just loops).
        let mut flow = at_prompt(config());
        flow.step(at(0), respond("pw"));
        let cmds = flow.step(at(0), err(false, "no such user"));
        assert!(has(&cmds, &GreetCmd::ShowError("no such user".into())));
        assert!(
            !has(&cmds, &GreetCmd::CancelSession),
            "a refused request must not reopen the same conversation: {cmds:?}"
        );
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
    }

    #[test]
    fn busy_is_a_level_not_a_balanced_pair() {
        // Documented so an adapter does not implement it as a refcount.
        let (mut flow, boot) = GreetFlow::new(at(0), kiosk());
        let mut busy: Vec<bool> = boot
            .iter()
            .filter_map(|c| match c {
                GreetCmd::SetBusy(on) => Some(*on),
                _ => None,
            })
            .collect();
        for event in [prompt("Password:", true), respond("ok"), ok(), ok()] {
            busy.extend(flow.step(at(0), event).iter().filter_map(|c| match c {
                GreetCmd::SetBusy(on) => Some(*on),
                _ => None,
            }));
        }
        assert_eq!(busy, vec![true, false, true, false], "{busy:?}");
    }

    #[test]
    fn the_typed_password_never_reaches_a_debug_rendering() {
        // vigil-sim already traces `{event:?}`; a derived Debug here is one
        // eprintln! away from putting passwords in the journal.
        let event = respond("hunter2");
        assert!(!format!("{event:?}").contains("hunter2"), "{event:?}");
        let mut flow = at_prompt(kiosk());
        let cmds = flow.step(at(0), respond("hunter2"));
        assert!(!format!("{cmds:?}").contains("hunter2"), "{cmds:?}");
        // The command still carries it for the adapter to post.
        assert!(has(&cmds, &GreetCmd::PostResponse(Some("hunter2".into()))));
    }
}
