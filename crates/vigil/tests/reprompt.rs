#![forbid(unsafe_code)]
use std::cell::RefCell;
use std::rc::Rc;

use vigil_core::{AuthUi, InputEvent, OutputId, Secret, UiMessage};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const RETURN: u32 = 0xff0d;

fn key(window: &mut OutputWindow, keysym: u32, utf8: Option<&str>) {
    for pressed in [true, false] {
        window.dispatch(InputEvent::Key {
            keysym,
            utf8: utf8.map(str::to_owned),
            pressed,
        });
    }
}

fn type_text(window: &mut OutputWindow, text: &str) {
    for c in text.chars() {
        key(window, c as u32, Some(&c.to_string()));
    }
}

#[test]
fn a_same_text_reprompt_keeps_what_is_typed_and_a_reset_clears_it() {
    let platform = VigilPlatform::install().expect("platform");
    let component = Theme::load_or_default(None)
        .instantiate()
        .expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let mut window =
        OutputWindow::new(OutputId(1), 1280, 800, 1.0, adapter, component).expect("window");
    let sent: Rc<RefCell<Vec<Secret>>> = Rc::default();
    let sink = sent.clone();
    window.on_ui_message(Rc::new(move |message| {
        if let UiMessage::Respond(text) = message {
            sink.borrow_mut().push(text);
        }
    }));
    window.set_panel_visible(true);

    window.show_prompt("Password:", true);
    type_text(&mut window, "hun");
    window.show_prompt("Password:", true);
    type_text(&mut window, "ter2");
    key(&mut window, RETURN, None);

    window.show_prompt("Password:", true);
    type_text(&mut window, "x");
    window.reset_input();
    window.show_prompt("Password:", true);
    type_text(&mut window, "pw");
    key(&mut window, RETURN, None);

    assert_eq!(
        *sent.borrow(),
        [Secret::from("hunter2"), Secret::from("pw")]
    );
}
