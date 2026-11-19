use vigil_auth::{AuthMachine, Prompt};
use vigil_config::{FirstQuestion, State};

use crate::autologin;

// libpam's PAM_MAX_MSG_SIZE. A longer question is left out, never cut:
// a remembered question is matched word for word against what PAM asks.
const MAX_QUESTION_BYTES: usize = 512;

pub fn seed(auth: &mut AuthMachine, state: &State) {
    auth.set_unprompted(Some(state.unprompted_user.clone()).filter(|user| !user.is_empty()));
    auth.set_first_questions(
        state
            .first_question
            .iter()
            .filter(|question| question.text.len() <= MAX_QUESTION_BYTES)
            .map(|question| {
                let prompt = Prompt {
                    text: question.text.clone(),
                    secret: question.secret,
                };
                (question.user.clone(), prompt)
            })
            .collect(),
    );
}

pub fn observe(state: &mut State, auth: &AuthMachine) -> bool {
    let before = state.clone();
    if auth.unprompted().is_none() {
        state.unprompted_user.clear();
    }
    // A name typed at the name stage may be a mistyped password, so it
    // reaches the state file only through `signed_in`.
    if !auth.typed_user() {
        record_first_question(state, auth);
    }
    *state != before
}

pub fn signed_in(mut state: State, auth: &AuthMachine, session: Option<&str>) -> State {
    if let (Some(user), Some(session)) = (auth.user(), session) {
        state.user = user.to_owned();
        state.session = session.to_owned();
    }
    state.unprompted_user = autologin::unprompted_user(auth.user(), auth.spoke());
    record_first_question(&mut state, auth);
    state
}

fn record_first_question(state: &mut State, auth: &AuthMachine) {
    let Some(user) = auth.user() else {
        return;
    };
    let question = auth
        .first_question()
        .filter(|prompt| !prompt.is_plain_password() && prompt.text.len() <= MAX_QUESTION_BYTES)
        .map(|prompt| FirstQuestion {
            user: user.to_owned(),
            text: prompt.text.clone(),
            secret: prompt.secret,
        });
    let stored = state.first_question.iter().filter(|q| q.user == user);
    if stored.eq(question.iter()) {
        return;
    }
    state.first_question.retain(|q| q.user != user);
    state.first_question.extend(question);
}

#[cfg(test)]
mod tests {
    use greetd_ipc::{AuthMessageType, ErrorType, Response};
    use vigil_core::{AuthUi, UiMessage};

    use super::*;
    use crate::scripted_greetd::Greetd;

    #[derive(Default)]
    struct Fields(Vec<String>);

    impl AuthUi for Fields {
        fn show_prompt(&mut self, text: &str, _secret: bool) {
            self.0.push(text.to_owned());
        }
        fn show_info(&mut self, _text: &str) {}
        fn show_error(&mut self, _text: &str) {}
        fn set_busy(&mut self, _busy: bool) {}
        fn reset_input(&mut self) {}
    }

    struct Start {
        auth: AuthMachine,
        state: State,
        ui: Fields,
    }

    impl Start {
        fn new(greetd: &Greetd) -> Self {
            let mut auth = AuthMachine::connect(greetd.socket().to_str()).expect("connect");
            auth.set_session(vec!["session".into()], Vec::new());
            let stored = State::load(&greetd.state_file());
            if let Some(state) = &stored {
                seed(&mut auth, state);
            }
            Self {
                auth,
                state: stored.unwrap_or_default(),
                ui: Fields::default(),
            }
        }

        fn choose(&mut self, user: &str) {
            let _ = self.auth.select_user(Some(user), &mut self.ui);
        }

        fn submit(&mut self, text: &str) {
            let _ = self
                .auth
                .handle(UiMessage::Respond(text.into()), &mut self.ui);
        }

        fn observed(&mut self, greetd: &Greetd) -> bool {
            let changed = observe(&mut self.state, &self.auth);
            if changed {
                self.state.store(&greetd.state_file());
            }
            changed
        }

        fn finished(self, greetd: &Greetd, session: Option<&str>) {
            assert!(self.auth.is_complete());
            signed_in(self.state, &self.auth, session).store(&greetd.state_file());
        }
    }

    fn asks(kind: AuthMessageType, text: &str) -> Response {
        Response::AuthMessage {
            auth_message_type: kind,
            auth_message: text.into(),
        }
    }

    fn refusal() -> Response {
        Response::Error {
            error_type: ErrorType::AuthError,
            description: "pam_authenticate: AUTH_ERR".into(),
        }
    }

    fn on_disk(greetd: &Greetd) -> State {
        State::load(&greetd.state_file()).unwrap_or_default()
    }

    fn question(user: &str, text: &str, secret: bool) -> FirstQuestion {
        FirstQuestion {
            user: user.into(),
            text: text.into(),
            secret,
        }
    }

    const PIN: &str = "Security token PIN: ";

    #[test]
    fn the_unasked_record_is_off_disk_while_its_sign_in_is_open_and_back_after_success() {
        let Some(greetd) = Greetd::scripted(
            "unprompted-claimed",
            vec![Response::Success, Response::Success],
        ) else {
            return;
        };
        State {
            user: "alice".into(),
            unprompted_user: "alice".into(),
            ..State::default()
        }
        .store(&greetd.state_file());
        let mut start = Start::new(&greetd);
        assert_eq!(
            on_disk(&greetd).unprompted_user,
            "",
            "a greeter that dies mid sign-in must not open the user unasked again"
        );
        assert_eq!(on_disk(&greetd).user, "alice");
        start.choose("alice");
        start.observed(&greetd);
        start.finished(&greetd, Some("session"));
        assert_eq!(on_disk(&greetd).unprompted_user, "alice");
        assert_eq!(greetd.finish(), ["create alice", "start"]);
    }

    #[test]
    fn a_question_put_to_the_unprompted_user_takes_the_record_out_of_the_state_file() {
        let Some(greetd) = Greetd::scripted(
            "unprompted-asked",
            vec![asks(AuthMessageType::Secret, "Password: ")],
        ) else {
            return;
        };
        State {
            user: "alice".into(),
            unprompted_user: "alice".into(),
            ..State::default()
        }
        .store(&greetd.state_file());
        let mut start = Start::new(&greetd);
        start.choose("alice");
        assert!(start.observed(&greetd));
        assert_eq!(on_disk(&greetd).unprompted_user, "");
        assert_eq!(on_disk(&greetd).user, "alice");
        drop(start);
        assert_eq!(greetd.finish(), ["create alice"]);
    }

    #[test]
    fn an_unasked_sign_in_refused_in_silence_takes_the_record_out_of_the_state_file() {
        let Some(greetd) =
            Greetd::scripted("unprompted-refused", vec![refusal(), Response::Success])
        else {
            return;
        };
        State {
            unprompted_user: "alice".into(),
            ..State::default()
        }
        .store(&greetd.state_file());
        let mut start = Start::new(&greetd);
        start.choose("alice");
        assert!(start.observed(&greetd));
        assert_eq!(on_disk(&greetd).unprompted_user, "");
        drop(start);

        let Some(next) = Greetd::scripted("unprompted-refused-next", vec![refusal()]) else {
            return;
        };
        on_disk(&greetd).store(&next.state_file());
        let mut start = Start::new(&next);
        start.choose("alice");
        drop(start);
        assert_eq!(next.finish(), [""; 0], "the next start waits for a submit");
        assert_eq!(greetd.finish(), ["create alice", "cancel"]);
    }

    #[test]
    fn a_sign_in_that_asked_nothing_is_recorded_and_opened_unasked_on_the_next_start() {
        let Some(greetd) = Greetd::scripted(
            "unprompted-written",
            vec![Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut start = Start::new(&greetd);
        start.choose("alice");
        start.submit("");
        assert!(!start.observed(&greetd));
        start.finished(&greetd, Some("Test DE"));
        let stored = on_disk(&greetd);
        assert_eq!(
            (stored.user.as_str(), stored.session.as_str()),
            ("alice", "Test DE")
        );
        assert_eq!(stored.unprompted_user, "alice");

        let Some(next) = Greetd::scripted(
            "unprompted-written-next",
            vec![Response::Success, Response::Success],
        ) else {
            return;
        };
        stored.store(&next.state_file());
        let mut start = Start::new(&next);
        start.choose("alice");
        assert!(start.auth.is_complete());
        drop(start);
        assert_eq!(next.finish(), ["create alice", "start"]);
        greetd.finish();
    }

    #[test]
    fn a_sign_in_that_asked_a_question_is_not_recorded_as_unprompted() {
        let Some(greetd) = Greetd::scripted(
            "prompted",
            vec![
                asks(AuthMessageType::Secret, "Password: "),
                Response::Success,
                Response::Success,
            ],
        ) else {
            return;
        };
        State {
            unprompted_user: "bob".into(),
            ..State::default()
        }
        .store(&greetd.state_file());
        let mut start = Start::new(&greetd);
        start.choose("alice");
        start.submit("hunter2");
        start.finished(&greetd, Some("Test DE"));
        let stored = on_disk(&greetd);
        assert_eq!(stored.user, "alice");
        assert_eq!(
            stored.unprompted_user, "",
            "one record: a sign-in by anyone else ends it"
        );
        assert_eq!(stored.first_question, Vec::new());
        greetd.finish();
    }

    #[test]
    fn the_unprompted_record_is_written_even_when_the_last_user_is_not_remembered() {
        let Some(greetd) =
            Greetd::scripted("not-remembered", vec![Response::Success, Response::Success])
        else {
            return;
        };
        let mut start = Start::new(&greetd);
        start.choose("alice");
        start.submit("");
        start.finished(&greetd, None);
        let stored = on_disk(&greetd);
        assert_eq!((stored.user.as_str(), stored.session.as_str()), ("", ""));
        assert_eq!(stored.unprompted_user, "alice");
        greetd.finish();
    }

    #[test]
    fn a_first_question_is_stored_on_sight_and_answered_first_try_on_the_next_start() {
        let Some(greetd) = Greetd::scripted("pin-first", vec![asks(AuthMessageType::Secret, PIN)])
        else {
            return;
        };
        let mut start = Start::new(&greetd);
        start.choose("alice");
        assert_eq!(start.ui.0, ["Password"]);
        start.submit("hunter2");
        assert_eq!(start.ui.0, ["Password", PIN]);
        assert!(start.observed(&greetd));
        assert!(!start.observed(&greetd));
        assert_eq!(
            on_disk(&greetd).first_question,
            [question("alice", PIN, true)]
        );
        drop(start);

        let Some(next) = Greetd::scripted(
            "pin-next",
            vec![
                asks(AuthMessageType::Secret, PIN),
                Response::Success,
                Response::Success,
            ],
        ) else {
            return;
        };
        on_disk(&greetd).store(&next.state_file());
        let mut start = Start::new(&next);
        start.choose("alice");
        assert_eq!(start.ui.0, [PIN]);
        assert_eq!(next.requests(), [""; 0]);
        start.submit("123456");
        assert!(!start.observed(&next));
        start.finished(&next, Some("Test DE"));
        assert_eq!(
            on_disk(&next).first_question,
            [question("alice", PIN, true)]
        );
        assert_eq!(next.finish(), ["create alice", "answer 123456", "start"]);
        assert_eq!(
            greetd.finish(),
            ["create alice"],
            "the password typed under Password never reached the PIN question"
        );
    }

    #[test]
    fn a_stored_question_pam_replaced_with_the_plain_password_is_removed() {
        let Some(greetd) = Greetd::scripted(
            "pin-gone",
            vec![asks(AuthMessageType::Secret, "Password: ")],
        ) else {
            return;
        };
        State {
            first_question: vec![
                question("alice", PIN, true),
                question("bob", "Code: ", false),
            ],
            ..State::default()
        }
        .store(&greetd.state_file());
        let mut start = Start::new(&greetd);
        start.choose("alice");
        start.submit("123456");
        assert!(start.observed(&greetd));
        assert_eq!(
            on_disk(&greetd).first_question,
            [question("bob", "Code: ", false)]
        );
        drop(start);
        assert_eq!(greetd.finish(), ["create alice"]);
    }

    #[test]
    fn a_typed_name_reaches_the_state_file_only_once_it_has_signed_in() {
        let Some(greetd) = Greetd::scripted(
            "typed-name",
            vec![
                asks(AuthMessageType::Secret, "Passphrase for hunter2: "),
                Response::Success,
                Response::Success,
            ],
        ) else {
            return;
        };
        let mut start = Start::new(&greetd);
        let _ = start.auth.begin(None, &mut start.ui);
        start.submit("hunter2");
        start.submit("");
        assert_eq!(greetd.requests(), ["create hunter2"]);
        assert!(!start.observed(&greetd));
        assert_eq!(
            std::fs::read_to_string(greetd.state_file()).unwrap_or_default(),
            "",
            "a name that may be a mistyped password is not written on sight"
        );
        start.submit("secret");
        start.finished(&greetd, None);
        assert_eq!(
            on_disk(&greetd).first_question,
            [question("hunter2", "Passphrase for hunter2: ", true)]
        );
        greetd.finish();
    }

    #[test]
    fn a_question_longer_than_a_pam_message_is_neither_stored_nor_offered() {
        let long = "x".repeat(MAX_QUESTION_BYTES + 1);
        let Some(greetd) =
            Greetd::scripted("long-question", vec![asks(AuthMessageType::Secret, &long)])
        else {
            return;
        };
        State {
            first_question: vec![question("bob", &long, true)],
            ..State::default()
        }
        .store(&greetd.state_file());
        let mut start = Start::new(&greetd);
        start.choose("bob");
        assert_eq!(start.ui.0, ["Password"]);
        start.choose("alice");
        start.submit("hunter2");
        start.observed(&greetd);
        assert!(
            on_disk(&greetd)
                .first_question
                .iter()
                .all(|stored| stored.user != "alice")
        );
        drop(start);
        assert_eq!(greetd.finish(), ["create alice"]);
    }
}
