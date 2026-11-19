use vigil_core::InputEvent;

const VT_FIRST: u32 = 0x1008_FE01;
const VT_LAST: u32 = 0x1008_FE0C;
const ESCAPE: u32 = 0xff1b;

pub trait KeySink {
    fn change_vt(&mut self, vt: i32);
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
        return;
    }
    if keysym == ESCAPE {
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
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use greetd_ipc::codec::SyncCodec;
    use greetd_ipc::{AuthMessageType, Request, Response};
    use vigil_auth::AuthMachine;
    use vigil_core::{OutputId, UiMessage};
    use vigil_theme::Theme;
    use vigil_ui::{OutputWindow, VigilPlatform};

    use super::*;

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
        assert!(sink.forwarded.is_empty());
    }

    struct Greetd {
        dir: PathBuf,
        requests: Arc<Mutex<Vec<String>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Greetd {
        fn scripted(name: &str, replies: Vec<Response>) -> Option<Self> {
            let dir =
                std::env::temp_dir().join(format!("vigil-keys-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let listener = match UnixListener::bind(dir.join("greetd.sock")) {
                Ok(listener) => listener,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    std::fs::remove_dir_all(&dir).unwrap();
                    return None;
                }
                Err(error) => panic!("bind the scripted greetd socket: {error}"),
            };
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let thread = std::thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                for reply in replies {
                    let Ok(request) = Request::read_from(&mut stream) else {
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
                    if reply.write_to(&mut stream).is_err() {
                        return;
                    }
                }
            });
            Some(Self {
                dir,
                requests,
                thread: Some(thread),
            })
        }

        fn finish(mut self) -> Vec<String> {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
            self.requests.lock().unwrap().clone()
        }
    }

    struct Rig {
        window: OutputWindow,
        queue: Rc<RefCell<VecDeque<UiMessage>>>,
        auth: AuthMachine,
    }

    impl Rig {
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
            let socket = greetd.dir.join("greetd.sock");
            let mut auth = AuthMachine::connect(socket.to_str()).expect("connect");
            auth.set_session(vec!["session".into()], Vec::new());
            Self {
                window,
                queue,
                auth,
            }
        }

        fn tick(&mut self) {
            loop {
                let message = self.queue.borrow_mut().pop_front();
                let Some(message) = message else { break };
                let _ = self.auth.handle(message, &mut self.window);
            }
        }
    }

    impl KeySink for Rig {
        fn change_vt(&mut self, _vt: i32) {}
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
    fn escape_a_name_and_a_password_in_one_batch_sign_that_user_in() {
        let Some(greetd) = Greetd::scripted(
            "escape",
            vec![
                secret(),
                Response::Success,
                secret(),
                Response::Success,
                Response::Success,
            ],
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
        assert_eq!(
            greetd.finish(),
            ["create alice", "cancel", "create bob", "answer pw", "start"]
        );
        assert!(complete);
    }
}
