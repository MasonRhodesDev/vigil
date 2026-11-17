use std::time::Duration;

use vigil_core::{PowerAction, UiMessage};

use crate::Now;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub const USERNAME_PROMPT: &str = "Username";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreetdReply {
    Prompt { text: String, secret: bool },
    Info(String),
    Notice(String),
    Success,
    Error { auth: bool, description: String },
}

#[derive(Clone, PartialEq)]
pub enum GreetEvent {
    Ui(UiMessage),
    GreetdReply(GreetdReply),
    GreetdLost(String),
    Tick,
}

#[derive(Clone, PartialEq)]
pub enum GreetCmd {
    ShowPrompt { text: String, secret: bool },
    ShowInfo(String),
    ShowError(String),
    SetBusy(bool),
    CreateSession(String),
    PostResponse(Option<String>),
    StartSession { cmd: Vec<String>, env: Vec<String> },
    CancelSession,
    SelectedUser { index: usize, user: Option<String> },
    SelectedSession(usize),
    Power(PowerAction),
    Exit(GreetOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreetOutcome {
    Started,
    Fatal(String),
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    None,
    Auth,
    Start,
    CancelThen(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreetPhase {
    Idle,
    AwaitingUser,
    AwaitingPrompt,
    Busy,
    Complete,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionChoice {
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GreetConfig {
    pub fixed_user: Option<String>,
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
    default_user: Option<String>,
    config: GreetConfig,
    session: SessionChoice,
    pending_error: Option<String>,
    deadline: Option<Duration>,
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

    pub fn session(&self) -> &SessionChoice {
        &self.session
    }

    pub fn next_wake(&self) -> Option<Duration> {
        self.wait
    }

    pub fn step(&mut self, now: Now, event: GreetEvent) -> Vec<GreetCmd> {
        let mut cmds = Vec::new();
        if self.phase == GreetPhase::Complete {
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
                _ => {}
            },
            UiMessage::Cancel => match self.phase {
                GreetPhase::AwaitingPrompt => self.cancel_then(now, None, cmds),
                GreetPhase::Idle => self.recover(now, cmds),
                _ => {}
            },
            UiMessage::SelectUser(index) => {
                if self.fixed_user {
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
            Pending::None => {}
            Pending::CancelThen(user) => match user {
                Some(user) => self.create_session(now, user, cmds),
                None => self.recover(now, cmds),
            },
            Pending::Start => match reply {
                GreetdReply::Success => {
                    self.phase = GreetPhase::Complete;
                    cmds.push(GreetCmd::SetBusy(false));
                    cmds.push(GreetCmd::Exit(GreetOutcome::Started));
                }
                GreetdReply::Error { description, .. } => {
                    cmds.push(GreetCmd::SetBusy(false));
                    cmds.push(GreetCmd::ShowError(description));
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
                    cmds.push(GreetCmd::ShowError(description.clone()));
                    if !auth {
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

    fn cancel_then(&mut self, now: Now, user: Option<String>, cmds: &mut Vec<GreetCmd>) {
        let user = user.or_else(|| self.fixed_user.then(|| self.user.clone()).flatten());
        cmds.push(GreetCmd::CancelSession);
        self.pending = Pending::CancelThen(user);
        self.phase = GreetPhase::Busy;
        self.deadline = Some(now.elapsed.saturating_add(REQUEST_TIMEOUT));
    }

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
        flow.step(at(0), GreetEvent::Ui(UiMessage::SelectUser(2)));
        let (mut flow2, _) = GreetFlow::new(at(0), config());
        let cmds = flow2.step(at(0), respond("   "));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
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
        let mut flow = at_prompt(kiosk());
        let cmds = flow.step(at(0), GreetEvent::Ui(UiMessage::Cancel));
        assert!(has(&cmds, &GreetCmd::CancelSession));
        assert!(
            !cmds.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))),
            "the reopen must wait for the cancel's reply: {cmds:?}"
        );
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
        let mut flow = at_prompt(config());
        flow.step(at(0), respond("ok"));
        flow.step(at(0), ok());
        let cmds = flow.step(at(0), err(false, "no such session"));
        assert!(has(&cmds, &GreetCmd::ShowError("no such session".into())));
        assert!(!has(&cmds, &GreetCmd::Exit(GreetOutcome::Started)));
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
        assert!(flow.step(at(0), ok()).is_empty());
    }

    #[test]
    fn construction_arms_a_wake_for_an_autologin_request() {
        let (flow, _) = GreetFlow::new(at(0), kiosk());
        assert!(
            flow.next_wake().is_some(),
            "no deadline armed at construction"
        );
        let (flow, _) = GreetFlow::new(at(0), config());
        assert!(flow.next_wake().is_none());
    }

    #[test]
    fn a_silent_greetd_does_not_wedge_the_greeter() {
        let mut flow = at_prompt(kiosk());
        flow.step(at(0), respond("hunter2"));
        assert!(flow.next_wake().is_some(), "no deadline armed");
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
        assert!(flow.next_wake().is_some());
    }

    #[test]
    fn a_non_auth_error_does_not_loop_forever() {
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
        let event = respond("hunter2");
        assert!(!format!("{event:?}").contains("hunter2"), "{event:?}");
        let mut flow = at_prompt(kiosk());
        let cmds = flow.step(at(0), respond("hunter2"));
        assert!(!format!("{cmds:?}").contains("hunter2"), "{cmds:?}");
        assert!(has(&cmds, &GreetCmd::PostResponse(Some("hunter2".into()))));
    }
}
