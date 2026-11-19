#![forbid(unsafe_code)]
use std::cell::RefCell;
use std::rc::Rc;

use vigil_core::{AuthUi, InputEvent, KeyText, OutputId, UiMessage};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const RETURN: u32 = 0xff0d;

fn window(id: u32) -> (OutputWindow, Rc<RefCell<Vec<UiMessage>>>) {
    let platform = VigilPlatform::install().expect("platform");
    let component = Theme::load_or_default(None)
        .instantiate()
        .expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let window =
        OutputWindow::new(OutputId(id), 1280, 800, 1.0, adapter, component).expect("window");
    let sent = Rc::new(RefCell::new(Vec::new()));
    let sink = sent.clone();
    window.on_ui_message(Rc::new(move |message| sink.borrow_mut().push(message)));
    (window, sent)
}

fn tap(window: &mut OutputWindow, keysym: u32, utf8: Option<&str>) {
    for pressed in [true, false] {
        window.dispatch(InputEvent::Key {
            keysym,
            utf8: utf8.map(KeyText::from),
            pressed,
        });
    }
}

fn type_text(window: &mut OutputWindow, text: &str) {
    for ch in text.chars() {
        tap(window, ch as u32, Some(&ch.to_string()));
    }
}

#[test]
fn a_new_prompt_starts_with_an_empty_field() {
    let (mut window, sent) = window(1);
    window.set_panel_visible(true);
    window.show_prompt("Password:", true);
    vigil_ui::advance_timers();
    type_text(&mut window, "stale");

    window.show_prompt("Username", false);
    type_text(&mut window, "bob");
    tap(&mut window, RETURN, None);

    assert_eq!(*sent.borrow(), [UiMessage::Respond("bob".into())]);
}

#[test]
fn text_typed_after_a_prompt_change_survives_the_next_tick() {
    let (mut window, sent) = window(2);
    window.set_panel_visible(true);
    window.show_prompt("Username", false);
    vigil_ui::advance_timers();

    window.show_prompt("Password:", true);
    type_text(&mut window, "sec");
    vigil_ui::advance_timers();
    type_text(&mut window, "ret");
    tap(&mut window, RETURN, None);

    assert_eq!(*sent.borrow(), [UiMessage::Respond("secret".into())]);
}
