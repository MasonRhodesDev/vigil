use vigil_core::InputEvent;

const VT_FIRST: u32 = 0x1008_FE01;
const VT_LAST: u32 = 0x1008_FE0C;
const ESCAPE: u32 = 0xff1b;

pub trait KeySink {
    fn change_vt(&mut self, vt: i32);
    fn field_focused(&self) -> bool;
    fn cancel(&mut self);
    fn forward(&mut self, event: InputEvent);
    fn settle(&mut self);
}

pub fn route(sink: &mut impl KeySink, event: InputEvent) {
    let InputEvent::Key {
        keysym, pressed, ..
    } = event
    else {
        return;
    };
    if pressed && (VT_FIRST..=VT_LAST).contains(&keysym) {
        sink.change_vt((keysym - VT_FIRST + 1) as i32);
        for release in vigil_core::modifier_releases() {
            sink.forward(release);
        }
        sink.settle();
        return;
    }
    if keysym == ESCAPE && sink.field_focused() {
        if pressed {
            sink.cancel();
            sink.settle();
        }
        return;
    }
    sink.forward(event);
    sink.settle();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use greetd_ipc::{AuthMessageType, Response};
    use vigil_auth::AuthMachine;
    use vigil_core::{OutputId, UiMessage};
    use vigil_theme::Theme;
    use vigil_ui::{OutputWindow, VigilPlatform};

    use super::*;
    use crate::scripted_greetd::Greetd;

    const RETURN: u32 = 0xff0d;

    fn tap(keysym: u32, utf8: Option<&str>) -> [InputEvent; 2] {
        [true, false].map(|pressed| InputEvent::Key {
            keysym,
            utf8: utf8.map(str::to_owned),
            pressed,
        })
    }

    fn text(text: &str) -> Vec<InputEvent> {
        text.chars()
            .flat_map(|ch| tap(ch as u32, Some(&ch.to_string())))
            .collect()
    }

    #[derive(Default)]
    struct Recorder {
        vts: Vec<i32>,
        cancels: usize,
        forwarded: Vec<(u32, bool)>,
    }

    impl KeySink for Recorder {
        fn change_vt(&mut self, vt: i32) {
            self.vts.push(vt);
        }
        fn field_focused(&self) -> bool {
            true
        }
        fn cancel(&mut self) {
            self.cancels += 1;
        }
        fn forward(&mut self, event: InputEvent) {
            if let InputEvent::Key {
                keysym, pressed, ..
            } = event
            {
                self.forwarded.push((keysym, pressed));
            }
        }
        fn settle(&mut self) {}
    }

    #[test]
    fn escape_cancels_and_never_reaches_the_window() {
        let mut sink = Recorder::default();
        for event in tap(ESCAPE, None).into_iter().chain(text("a")) {
            route(&mut sink, event);
        }
        assert_eq!(sink.cancels, 1);
        assert_eq!(sink.forwarded, [('a' as u32, true), ('a' as u32, false)]);
    }

    #[test]
    fn a_vt_key_switches_the_terminal() {
        let mut sink = Recorder::default();
        route(&mut sink, tap(VT_FIRST + 2, None)[0].clone());
        assert_eq!(sink.vts, [3]);
        assert!(
            sink.forwarded.iter().all(|(_, pressed)| !pressed),
            "only modifier releases follow a VT switch"
        );
    }

    struct Rig {
        window: OutputWindow,
        queue: Rc<RefCell<VecDeque<UiMessage>>>,
        auth: AuthMachine,
        users: Vec<String>,
    }

    impl Rig {
        fn with_pickers(greetd: &Greetd) -> Self {
            let mut rig = Self::new(greetd);
            rig.users = vec!["alice".into(), "bob".into()];
            rig.window.set_users(&rig.users);
            rig.window.set_user_index(0);
            rig.window.set_user_name("alice");
            rig.window
                .set_sessions(&["Hyprland".into(), "Shell".into()]);
            rig.window.set_session_index(0);
            rig
        }

        fn new(greetd: &Greetd) -> Self {
            let platform = VigilPlatform::install().expect("platform");
            let component = Theme::load_or_default(None)
                .instantiate()
                .expect("instantiate");
            let adapter = platform.claim_last_adapter().expect("adapter");
            let mut window =
                OutputWindow::new(OutputId(1), 1280, 800, 1.0, adapter, component).expect("window");
            window.set_panel_visible(true);
            let queue = Rc::new(RefCell::new(VecDeque::new()));
            let sink = queue.clone();
            window.on_ui_message(Rc::new(move |message| sink.borrow_mut().push_back(message)));
            let mut auth = AuthMachine::connect(greetd.socket().to_str()).expect("connect");
            auth.set_session(vec!["session".into()], Vec::new());
            Self {
                window,
                queue,
                auth,
                users: Vec::new(),
            }
        }

        fn tick(&mut self) {
            loop {
                let message = self.queue.borrow_mut().pop_front();
                let Some(message) = message else { break };
                match message {
                    UiMessage::SelectUser(index) => {
                        self.window.set_user_index(index);
                        let user = self.users.get(index).cloned();
                        self.auth
                            .select_user(user.as_deref(), &mut self.window)
                            .expect("select user");
                    }
                    UiMessage::SelectSession(index) => self.window.set_session_index(index),
                    UiMessage::Power(action) => panic!("the drive powered off: {action:?}"),
                    message => {
                        let _ = self.auth.handle(message, &mut self.window);
                    }
                }
            }
        }
    }

    impl KeySink for Rig {
        fn change_vt(&mut self, _vt: i32) {}
        fn field_focused(&self) -> bool {
            self.window.field_focused()
        }
        fn cancel(&mut self) {
            self.queue.borrow_mut().push_back(UiMessage::Cancel);
        }
        fn forward(&mut self, event: InputEvent) {
            self.window.dispatch(event);
        }
        fn settle(&mut self) {
            self.tick();
        }
    }

    fn secret() -> Response {
        Response::AuthMessage {
            auth_message_type: AuthMessageType::Secret,
            auth_message: "Password:".into(),
        }
    }

    #[test]
    fn a_password_begun_in_the_same_batch_as_the_name_is_kept() {
        let Some(greetd) = Greetd::scripted(
            "typed-ahead",
            vec![secret(), Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut rig = Rig::new(&greetd);
        rig.auth
            .begin(None, &mut rig.window)
            .expect("username prompt");

        let mut batch = text("bob");
        batch.extend(tap(RETURN, None));
        batch.extend(text("sec"));
        for event in batch {
            route(&mut rig, event);
        }
        rig.tick();
        let mut batch = text("ret");
        batch.extend(tap(RETURN, None));
        for event in batch {
            route(&mut rig, event);
        }
        rig.tick();

        drop(rig);
        assert_eq!(greetd.finish(), ["create bob", "answer secret", "start"]);
    }

    #[test]
    fn escape_clears_a_fixed_users_password_field() {
        let Some(greetd) = Greetd::scripted(
            "fixed-escape",
            vec![secret(), Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut rig = Rig::new(&greetd);
        rig.auth
            .begin(Some("alice"), &mut rig.window)
            .expect("first prompt");
        for event in text("x") {
            route(&mut rig, event);
        }
        rig.tick();
        for event in tap(ESCAPE, None) {
            route(&mut rig, event);
        }
        rig.tick();
        let mut batch = text("pw");
        batch.extend(tap(RETURN, None));
        for event in batch {
            route(&mut rig, event);
        }
        rig.tick();

        drop(rig);
        assert_eq!(greetd.finish(), ["create alice", "answer pw", "start"]);
    }

    #[test]
    fn escape_a_name_and_a_password_in_one_batch_sign_that_user_in() {
        let Some(greetd) = Greetd::scripted(
            "escape",
            vec![secret(), Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut rig = Rig::new(&greetd);
        rig.auth
            .select_user(Some("alice"), &mut rig.window)
            .expect("first prompt");
        for event in text("x") {
            route(&mut rig, event);
        }
        rig.tick();

        let mut batch = tap(ESCAPE, None).to_vec();
        batch.extend(text("bob"));
        batch.extend(tap(RETURN, None));
        batch.extend(text("pw"));
        batch.extend(tap(RETURN, None));
        for event in batch {
            route(&mut rig, event);
        }
        rig.tick();

        let complete = rig.auth.is_complete();
        drop(rig);
        assert_eq!(greetd.finish(), ["create bob", "answer pw", "start"]);
        assert!(complete);
    }

    const TAB: u32 = 0xff09;
    const SPACE: u32 = 0x20;
    const RIGHT: u32 = 0xff53;
    const F8: u32 = 0xffc5;
    const SHIFT_L: u32 = 0xffe1;
    const CONTROL_L: u32 = 0xffe3;
    const ALT_L: u32 = 0xffe9;

    fn press(keysym: u32) -> InputEvent {
        InputEvent::Key {
            keysym,
            utf8: None,
            pressed: true,
        }
    }

    fn release(keysym: u32) -> InputEvent {
        InputEvent::Key {
            keysym,
            utf8: None,
            pressed: false,
        }
    }

    fn every_control_by_keyboard() -> Vec<InputEvent> {
        let mut drive = vec![press(SHIFT_L)];
        drive.extend(tap(TAB, Some("\t")));
        drive.push(release(SHIFT_L));
        drive.extend(tap(RIGHT, None));
        drive.extend(tap(TAB, Some("\t")));
        drive.extend(tap(TAB, Some("\t")));
        drive.extend(tap(SPACE, Some(" ")));
        drive.extend(tap(TAB, Some("\t")));
        drive.extend(tap(RIGHT, None));
        drive.extend(tap(TAB, Some("\t")));
        drive.extend(tap(RETURN, None));
        drive.extend(tap(TAB, Some("\t")));
        drive.extend(tap(ESCAPE, None));
        drive.extend(tap(ESCAPE, None));
        drive.push(press(ALT_L));
        drive.extend(tap(F8, None));
        drive.push(release(ALT_L));
        drive.extend(text("pw"));
        drive
    }

    #[test]
    fn keyboard_switching_opens_no_conversation_before_enter() {
        let Some(greetd) = Greetd::scripted("keyboard-local", vec![secret()]) else {
            return;
        };
        let mut rig = Rig::with_pickers(&greetd);
        rig.auth
            .select_user(Some("alice"), &mut rig.window)
            .expect("first prompt");
        for event in every_control_by_keyboard() {
            route(&mut rig, event);
        }
        rig.tick();

        drop(rig);
        assert_eq!(greetd.finish(), Vec::<String>::new());
    }

    #[test]
    fn the_user_picked_by_keyboard_signs_in_on_enter() {
        let Some(greetd) = Greetd::scripted(
            "keyboard-pick",
            vec![secret(), Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut rig = Rig::with_pickers(&greetd);
        rig.auth
            .select_user(Some("alice"), &mut rig.window)
            .expect("first prompt");
        let mut drive = every_control_by_keyboard();
        drive.extend(tap(RETURN, None));
        for event in drive {
            route(&mut rig, event);
        }
        rig.tick();

        let complete = rig.auth.is_complete();
        drop(rig);
        assert_eq!(greetd.finish(), ["create bob", "answer pw", "start"]);
        assert!(complete);
    }

    #[test]
    fn control_z_after_escape_never_answers_with_the_cleared_password() {
        let Some(greetd) = Greetd::scripted(
            "escape-undo",
            vec![secret(), Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut rig = Rig::new(&greetd);
        rig.auth
            .begin(Some("alice"), &mut rig.window)
            .expect("first prompt");
        let mut drive = text("secret");
        drive.extend(tap(ESCAPE, None));
        drive.push(press(CONTROL_L));
        drive.extend(tap('z' as u32, Some("\u{1a}")));
        drive.push(release(CONTROL_L));
        drive.extend(tap(RETURN, None));
        for event in drive {
            route(&mut rig, event);
        }
        rig.tick();

        drop(rig);
        assert_eq!(greetd.finish(), ["create alice"]);
    }

    #[test]
    fn modifiers_held_across_a_vt_switch_do_not_stick() {
        let Some(greetd) = Greetd::scripted(
            "vt-modifiers",
            vec![secret(), Response::Success, Response::Success],
        ) else {
            return;
        };
        let mut rig = Rig::new(&greetd);
        rig.auth
            .select_user(Some("alice"), &mut rig.window)
            .expect("first prompt");
        let mut drive = vec![press(CONTROL_L), press(ALT_L), press(VT_FIRST + 1)];
        drive.extend(text("pw"));
        drive.extend(tap(RETURN, None));
        for event in drive {
            route(&mut rig, event);
        }
        rig.tick();

        drop(rig);
        assert_eq!(greetd.finish(), ["create alice", "answer pw", "start"]);
    }
}
