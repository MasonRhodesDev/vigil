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
//! command out and a [`GreetEvent::GreetdReply`] back in, so the whole
//! stage machine is exercisable without a socket or a seat.
//!
//! This is deliberately *not* merged with [`crate::LockFlow`]: the greeter
//! authenticates through greetd and ends by execing a session, the locker
//! authenticates through PAM directly and ends by releasing a lock. They
//! share this crate, not a machine.

use vigil_core::{PowerAction, UiMessage};

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
    /// The outstanding request failed.
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GreetEvent {
    /// A message from the UI (respond, cancel, select, power).
    Ui(UiMessage),
    /// greetd answered the request that was outstanding.
    GreetdReply(GreetdReply),
    /// The connection to greetd failed irrecoverably.
    GreetdLost(String),
}

#[derive(Debug, Clone, PartialEq)]
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
    /// The greeter's own selection state changed.
    SelectedUser(Option<usize>),
    SelectedSession(usize),
    Power(PowerAction),
    /// Terminal: the session is running and the greeter should exit.
    SessionStarted,
    /// Terminal failure the greeter cannot continue from.
    Fatal(String),
}

/// Which request is awaiting a reply. greetd is strictly request/response,
/// so the same `Success` means different things depending on what was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    Create,
    Respond,
    Start,
    /// A cancel issued to clear a failed conversation before reopening it.
    CancelThenCreate,
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

pub struct GreetFlow {
    phase: GreetPhase,
    pending: Pending,
    user: Option<String>,
    /// A user fixed by configuration (kiosk/autologin) cannot be switched.
    fixed_user: bool,
    default_user: Option<String>,
    session: SessionChoice,
    /// An auth failure is re-raised *after* the next prompt, or the user
    /// never sees why they are retyping.
    pending_error: Option<String>,
}

/// The session command/env the greeter will ask greetd to start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionChoice {
    pub index: usize,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

impl GreetFlow {
    /// `user` fixes the account (kiosk/autologin) and skips the username
    /// stage. `default_user` is the remembered account an empty submission
    /// falls back to.
    pub fn new(
        user: Option<String>,
        default_user: Option<String>,
        session: SessionChoice,
    ) -> (Self, Vec<GreetCmd>) {
        let mut flow = Self {
            phase: GreetPhase::Idle,
            pending: Pending::None,
            user: None,
            fixed_user: user.is_some(),
            default_user,
            session,
            pending_error: None,
        };
        let mut cmds = Vec::new();
        match user {
            Some(user) => flow.create_session(user, &mut cmds),
            None => flow.prompt_username(&mut cmds),
        }
        (flow, cmds)
    }

    pub fn phase(&self) -> GreetPhase {
        self.phase
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    pub fn step(&mut self, event: GreetEvent) -> Vec<GreetCmd> {
        let mut cmds = Vec::new();
        if self.phase == GreetPhase::Complete {
            // The session is running; nothing may reopen a conversation.
            return cmds;
        }
        match event {
            GreetEvent::Ui(message) => self.on_ui(message, &mut cmds),
            GreetEvent::GreetdReply(reply) => self.on_reply(reply, &mut cmds),
            GreetEvent::GreetdLost(error) => {
                self.pending = Pending::None;
                self.phase = GreetPhase::Idle;
                cmds.push(GreetCmd::SetBusy(false));
                cmds.push(GreetCmd::Fatal(error));
            }
        }
        cmds
    }

    fn on_ui(&mut self, message: UiMessage, cmds: &mut Vec<GreetCmd>) {
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
                        Some(user) => self.create_session(user, cmds),
                        None => self.prompt_username(cmds),
                    }
                }
                GreetPhase::AwaitingPrompt => {
                    self.pending = Pending::Respond;
                    self.phase = GreetPhase::Busy;
                    cmds.push(GreetCmd::SetBusy(true));
                    cmds.push(GreetCmd::PostResponse(Some(text)));
                }
                // A submission with nothing outstanding is a stray repeat.
                _ => {}
            },
            UiMessage::Cancel => {
                if self.phase == GreetPhase::AwaitingPrompt {
                    cmds.push(GreetCmd::CancelSession);
                    self.restart(cmds);
                }
            }
            UiMessage::SelectUser(index) => {
                if self.fixed_user {
                    // Configuration pinned the account; the picker is inert.
                    return;
                }
                cmds.push(GreetCmd::SelectedUser(Some(index)));
            }
            UiMessage::SelectSession(index) => {
                self.session.index = index;
                cmds.push(GreetCmd::SelectedSession(index));
            }
            UiMessage::Power(action) => cmds.push(GreetCmd::Power(action)),
        }
    }

    fn on_reply(&mut self, reply: GreetdReply, cmds: &mut Vec<GreetCmd>) {
        let pending = std::mem::replace(&mut self.pending, Pending::None);
        match (pending, reply) {
            (Pending::None, _) => {}
            // An auth message: show it and either wait, or answer the empty
            // response the protocol expects for info/error text.
            (_, GreetdReply::Prompt { text, secret }) => {
                cmds.push(GreetCmd::SetBusy(false));
                cmds.push(GreetCmd::ShowPrompt { text, secret });
                if let Some(error) = self.pending_error.take() {
                    cmds.push(GreetCmd::ShowError(error));
                }
                self.phase = GreetPhase::AwaitingPrompt;
            }
            (_, GreetdReply::Info(text)) => {
                cmds.push(GreetCmd::ShowInfo(text));
                self.pending = Pending::Respond;
                cmds.push(GreetCmd::PostResponse(None));
            }
            (_, GreetdReply::Notice(text)) => {
                cmds.push(GreetCmd::ShowError(text));
                self.pending = Pending::Respond;
                cmds.push(GreetCmd::PostResponse(None));
            }
            // Authentication finished — launch the session.
            (Pending::Create | Pending::Respond, GreetdReply::Success) => {
                self.pending = Pending::Start;
                cmds.push(GreetCmd::StartSession {
                    cmd: self.session.cmd.clone(),
                    env: self.session.env.clone(),
                });
            }
            (Pending::Start, GreetdReply::Success) => {
                self.phase = GreetPhase::Complete;
                cmds.push(GreetCmd::SetBusy(false));
                cmds.push(GreetCmd::SessionStarted);
            }
            (Pending::Start, GreetdReply::Error(description)) => {
                cmds.push(GreetCmd::SetBusy(false));
                cmds.push(GreetCmd::ShowError(description));
                self.phase = GreetPhase::Idle;
            }
            (Pending::CancelThenCreate, _) => {
                // The cancel's own reply is uninteresting; reopen.
                let Some(user) = self.user.clone() else {
                    self.prompt_username(cmds);
                    return;
                };
                self.create_session(user, cmds);
            }
            // A failed create/respond: re-raise after the next prompt so the
            // user sees why they are retyping.
            (Pending::Create | Pending::Respond, GreetdReply::Error(description)) => {
                cmds.push(GreetCmd::SetBusy(false));
                self.pending_error = Some(description);
                cmds.push(GreetCmd::CancelSession);
                self.pending = Pending::CancelThenCreate;
                self.phase = GreetPhase::Busy;
            }
        }
    }

    fn create_session(&mut self, user: String, cmds: &mut Vec<GreetCmd>) {
        self.user = Some(user.clone());
        self.pending = Pending::Create;
        self.phase = GreetPhase::Busy;
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

    /// Back to a fresh conversation for the same account, or to the username
    /// stage when there is none.
    fn restart(&mut self, cmds: &mut Vec<GreetCmd>) {
        match self.user.clone() {
            Some(user) => self.create_session(user, cmds),
            None => self.prompt_username(cmds),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionChoice {
        SessionChoice {
            index: 0,
            cmd: vec!["Hyprland".into()],
            env: vec!["XDG_SESSION_TYPE=wayland".into()],
        }
    }

    fn has(cmds: &[GreetCmd], wanted: &GreetCmd) -> bool {
        cmds.contains(wanted)
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

    #[test]
    fn the_username_stage_precedes_greetd() {
        let (mut flow, boot) = GreetFlow::new(None, None, session());
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
        assert!(has(
            &boot,
            &GreetCmd::ShowPrompt {
                text: USERNAME_PROMPT.into(),
                secret: false
            }
        ));
        // greetd is not contacted until a username is submitted.
        assert!(!boot.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))));
        let cmds = flow.step(respond("mason"));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
        assert_eq!(flow.user(), Some("mason"));
    }

    #[test]
    fn a_fixed_user_skips_the_username_stage_and_pins_the_picker() {
        let (mut flow, boot) = GreetFlow::new(Some("kiosk".into()), None, session());
        assert!(has(&boot, &GreetCmd::CreateSession("kiosk".into())));
        assert_eq!(flow.phase(), GreetPhase::Busy);
        // The account is configuration, not a choice.
        let cmds = flow.step(GreetEvent::Ui(UiMessage::SelectUser(2)));
        assert!(cmds.is_empty(), "{cmds:?}");
    }

    #[test]
    fn an_empty_submission_takes_the_remembered_account() {
        let (mut flow, _) = GreetFlow::new(None, Some("mason".into()), session());
        let cmds = flow.step(respond("   "));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
    }

    #[test]
    fn an_empty_submission_with_no_default_re_prompts() {
        let (mut flow, _) = GreetFlow::new(None, None, session());
        let cmds = flow.step(respond(""));
        assert!(!cmds.iter().any(|c| matches!(c, GreetCmd::CreateSession(_))));
        assert_eq!(flow.phase(), GreetPhase::AwaitingUser);
    }

    #[test]
    fn a_password_prompt_is_answered_and_starts_the_session() {
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        let cmds = flow.step(prompt("Password:", true));
        assert!(has(
            &cmds,
            &GreetCmd::ShowPrompt {
                text: "Password:".into(),
                secret: true
            }
        ));
        assert_eq!(flow.phase(), GreetPhase::AwaitingPrompt);

        let cmds = flow.step(respond("hunter2"));
        assert!(has(&cmds, &GreetCmd::PostResponse(Some("hunter2".into()))));
        assert!(has(&cmds, &GreetCmd::SetBusy(true)));

        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        assert!(has(
            &cmds,
            &GreetCmd::StartSession {
                cmd: vec!["Hyprland".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            }
        ));
        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        assert!(has(&cmds, &GreetCmd::SessionStarted));
        assert_eq!(flow.phase(), GreetPhase::Complete);
    }

    #[test]
    fn an_auth_failure_reopens_and_shows_why_after_the_new_prompt() {
        // The contract: a fresh prompt clears UI messages, so the failure is
        // re-raised AFTER it or the user never learns why they are retyping.
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        flow.step(prompt("Password:", true));
        flow.step(respond("wrong"));
        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Error(
            "authentication failed".into(),
        )));
        assert!(has(&cmds, &GreetCmd::CancelSession));
        // Not yet: the prompt has to land first.
        assert!(!has(
            &cmds,
            &GreetCmd::ShowError("authentication failed".into())
        ));

        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
        let cmds = flow.step(prompt("Password:", true));
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
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Info("hello".into())));
        assert!(has(&cmds, &GreetCmd::ShowInfo("hello".into())));
        // The protocol requires an empty response so greetd continues.
        assert!(has(&cmds, &GreetCmd::PostResponse(None)));
        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Notice(
            "beware".into(),
        )));
        assert!(has(&cmds, &GreetCmd::ShowError("beware".into())));
        assert!(has(&cmds, &GreetCmd::PostResponse(None)));
    }

    #[test]
    fn cancel_reopens_a_fresh_conversation() {
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        flow.step(prompt("Password:", true));
        let cmds = flow.step(GreetEvent::Ui(UiMessage::Cancel));
        assert!(has(&cmds, &GreetCmd::CancelSession));
        assert!(has(&cmds, &GreetCmd::CreateSession("mason".into())));
    }

    #[test]
    fn session_selection_is_carried_into_start_session() {
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        let cmds = flow.step(GreetEvent::Ui(UiMessage::SelectSession(3)));
        assert!(has(&cmds, &GreetCmd::SelectedSession(3)));
        flow.step(prompt("Password:", true));
        flow.step(respond("ok"));
        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, GreetCmd::StartSession { .. })),
            "{cmds:?}"
        );
    }

    #[test]
    fn power_actions_pass_through_at_any_stage() {
        let (mut flow, _) = GreetFlow::new(None, None, session());
        let cmds = flow.step(GreetEvent::Ui(UiMessage::Power(PowerAction::Poweroff)));
        assert!(has(&cmds, &GreetCmd::Power(PowerAction::Poweroff)));
    }

    #[test]
    fn a_started_session_ignores_everything_after() {
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        flow.step(prompt("Password:", true));
        flow.step(respond("ok"));
        flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        assert_eq!(flow.phase(), GreetPhase::Complete);
        assert!(flow.step(respond("again")).is_empty());
        assert!(flow.step(GreetEvent::Ui(UiMessage::Cancel)).is_empty());
    }

    #[test]
    fn a_failed_start_session_returns_to_idle_rather_than_exiting() {
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        flow.step(prompt("Password:", true));
        flow.step(respond("ok"));
        flow.step(GreetEvent::GreetdReply(GreetdReply::Success));
        let cmds = flow.step(GreetEvent::GreetdReply(GreetdReply::Error(
            "no such session".into(),
        )));
        assert!(has(&cmds, &GreetCmd::ShowError("no such session".into())));
        assert!(!has(&cmds, &GreetCmd::SessionStarted));
        assert_eq!(flow.phase(), GreetPhase::Idle);
    }

    #[test]
    fn a_lost_connection_is_fatal_and_clears_busy() {
        let (mut flow, _) = GreetFlow::new(Some("mason".into()), None, session());
        let cmds = flow.step(GreetEvent::GreetdLost("broken pipe".into()));
        assert!(has(&cmds, &GreetCmd::SetBusy(false)));
        assert!(has(&cmds, &GreetCmd::Fatal("broken pipe".into())));
    }
}
