use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use vigil_core::{AuthUi, FrameTarget, InputEvent, OutputId, PowerAction, UiMessage};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const TAB: u32 = 0xff09;
const ISO_LEFT_TAB: u32 = 0xfe20;
const RETURN: u32 = 0xff0d;
const ESCAPE: u32 = 0xff1b;
const LEFT: u32 = 0xff51;
const RIGHT: u32 = 0xff53;
const SPACE: u32 = 0x20;
const F8: u32 = 0xffc5;
const SHIFT_L: u32 = 0xffe1;
const CONTROL_L: u32 = 0xffe3;
const CONTROL_R: u32 = 0xffe4;
const ALT_L: u32 = 0xffe9;

const W: u32 = 1280;
const H: u32 = 800;

#[derive(Debug, PartialEq)]
enum Sent {
    Respond(String),
    Cancel,
    Session(usize),
    User(usize),
    Power(PowerAction),
}

struct Greeter {
    window: OutputWindow,
    log: Rc<RefCell<Vec<UiMessage>>>,
    sent: Vec<Sent>,
}

impl Greeter {
    fn with_theme(platform: VigilPlatform, theme: &Theme) -> Self {
        Self::sized(platform, theme, W, H)
    }

    fn sized(platform: VigilPlatform, theme: &Theme, width: u32, height: u32) -> Self {
        let component = theme.instantiate().expect("instantiate");
        let adapter = platform.claim_last_adapter().expect("adapter");
        let window =
            OutputWindow::new(OutputId(1), width, height, 1.0, adapter, component).expect("window");
        let log: Rc<RefCell<Vec<UiMessage>>> = Rc::default();
        let sink = log.clone();
        window.on_ui_message(Rc::new(move |message| sink.borrow_mut().push(message)));
        let mut greeter = Self {
            window,
            log,
            sent: Vec::new(),
        };
        greeter.window.set_panel_visible(true);
        greeter.window.show_prompt("Password:", true);
        greeter
    }

    fn new() -> Self {
        let platform = VigilPlatform::install().expect("platform");
        Self::with_theme(platform, &Theme::load_or_default(None))
    }

    fn with_pickers() -> Self {
        let mut greeter = Self::new();
        greeter
            .window
            .set_users(&["alice".into(), "bob".into(), "carol".into()]);
        greeter.window.set_user_index(0);
        greeter.window.set_user_name("alice");
        greeter
            .window
            .set_sessions(&["Hyprland".into(), "Shell".into()]);
        greeter.window.set_session_index(0);
        greeter
    }

    fn event(&mut self, keysym: u32, utf8: Option<&str>, pressed: bool) {
        self.window.dispatch(InputEvent::Key {
            keysym,
            utf8: utf8.map(str::to_owned),
            pressed,
        });
        self.settle();
    }

    fn tap(&mut self, keysym: u32) {
        let utf8 = match keysym {
            TAB => Some("\t"),
            SPACE => Some(" "),
            RETURN => Some("\r"),
            _ => None,
        };
        self.event(keysym, utf8, true);
        self.event(keysym, utf8, false);
    }

    fn chord(&mut self, modifier: u32, keysym: u32, utf8: Option<&str>) {
        self.event(modifier, None, true);
        self.event(keysym, utf8, true);
        self.event(keysym, utf8, false);
        self.event(modifier, None, false);
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            self.event(c as u32, Some(&s), true);
            self.event(c as u32, Some(&s), false);
        }
    }

    fn settle(&mut self) {
        let messages: Vec<UiMessage> = self.log.borrow_mut().drain(..).collect();
        for message in messages {
            self.sent.push(match message {
                UiMessage::Respond(text) => Sent::Respond(text),
                UiMessage::Cancel => Sent::Cancel,
                UiMessage::SelectSession(index) => {
                    self.window.set_session_index(index);
                    Sent::Session(index)
                }
                UiMessage::SelectUser(index) => {
                    self.window.set_user_index(index);
                    Sent::User(index)
                }
                UiMessage::Power(action) => Sent::Power(action),
            });
        }
    }

    fn take(&mut self) -> Vec<Sent> {
        std::mem::take(&mut self.sent)
    }
}

#[test]
fn tab_walks_every_control_in_visual_order_and_wraps() {
    let mut g = Greeter::with_pickers();

    g.tap(TAB);
    g.tap(TAB);
    g.tap(SPACE);
    assert_eq!(
        g.take(),
        [Sent::Session(1)],
        "field, show/hide, then session"
    );

    g.tap(TAB);
    g.tap(TAB);
    g.tap(SPACE);
    g.tap(TAB);
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Power(PowerAction::Poweroff)],
        "Restart, then Shut Down, then its confirm"
    );

    g.tap(TAB);
    g.tap(SPACE);
    assert_eq!(
        g.take(),
        [Sent::User(1)],
        "after Shut Down, Tab wraps to the user picker"
    );

    g.tap(TAB);
    g.type_text("pw");
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Respond("pw".into())],
        "the field follows the user picker"
    );
}

fn shift_tab_walks_backwards_from_the_field(shifted_tab: u32) {
    let mut g = Greeter::with_pickers();
    g.chord(SHIFT_L, shifted_tab, Some("\t"));
    g.tap(RIGHT);
    assert_eq!(
        g.take(),
        [Sent::User(1)],
        "Shift+Tab from the field reaches the user picker"
    );
    g.chord(SHIFT_L, shifted_tab, Some("\t"));
    g.tap(SPACE);
    g.tap(TAB);
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Power(PowerAction::Poweroff)],
        "Shift+Tab from the user picker reaches Shut Down"
    );
}

#[test]
fn iso_left_tab_walks_backwards() {
    shift_tab_walks_backwards_from_the_field(ISO_LEFT_TAB);
}

#[test]
fn tab_with_shift_held_walks_backwards() {
    shift_tab_walks_backwards_from_the_field(TAB);
}

#[test]
fn pickers_step_both_ways_and_enter_returns_to_the_field() {
    let mut g = Greeter::with_pickers();
    g.chord(SHIFT_L, ISO_LEFT_TAB, Some("\t"));
    g.tap(LEFT);
    g.tap(RIGHT);
    g.tap(RIGHT);
    g.tap(RETURN);
    g.type_text("pw");
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [
            Sent::User(2),
            Sent::User(0),
            Sent::User(1),
            Sent::Respond("pw".into())
        ]
    );

    g.tap(TAB);
    g.tap(TAB);
    g.tap(RIGHT);
    g.tap(LEFT);
    g.tap(RETURN);
    g.type_text("x");
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [
            Sent::Session(1),
            Sent::Session(0),
            Sent::Respond("x".into())
        ]
    );
}

#[test]
fn space_never_types_into_the_field_once_focus_has_left_it() {
    let mut g = Greeter::with_pickers();
    g.tap(TAB);
    g.tap(SPACE);
    g.tap(SPACE);
    g.tap(TAB);
    g.tap(SPACE);
    g.chord(SHIFT_L, ISO_LEFT_TAB, Some("\t"));
    g.chord(SHIFT_L, ISO_LEFT_TAB, Some("\t"));
    g.type_text("pw");
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Session(1), Sent::Respond("pw".into())],
        "no space reached the password"
    );
}

#[test]
fn typing_on_a_control_returns_to_the_field() {
    let mut g = Greeter::with_pickers();
    g.tap(TAB);
    g.tap(TAB);
    g.type_text("pw");
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("pw".into())]);
}

#[test]
fn escape_on_a_control_returns_to_the_field_without_cancelling() {
    let mut g = Greeter::with_pickers();
    g.tap(TAB);
    g.tap(TAB);
    g.tap(ESCAPE);
    g.tap(SPACE);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond(" ".into())]);
}

#[test]
fn power_needs_a_confirm_that_starts_on_cancel() {
    let mut g = Greeter::with_pickers();
    for _ in 0..4 {
        g.tap(TAB);
    }
    for _ in 0..9 {
        g.tap(RETURN);
    }
    for _ in 0..9 {
        g.tap(SPACE);
    }
    assert_eq!(
        g.take(),
        [],
        "mashing Enter or Space on Shut Down never powers off"
    );

    g.tap(ESCAPE);
    for _ in 0..4 {
        g.tap(TAB);
    }
    g.tap(SPACE);
    g.tap(TAB);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Power(PowerAction::Poweroff)]);
}

#[test]
fn escape_backs_out_of_a_power_confirm() {
    let mut g = Greeter::with_pickers();
    for _ in 0..3 {
        g.tap(TAB);
    }
    g.tap(RETURN);
    g.tap(TAB);
    g.tap(ESCAPE);
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [],
        "Esc disarms and leaves focus on Restart, so Enter only arms again"
    );
    g.tap(TAB);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Power(PowerAction::Reboot)]);
}

#[test]
fn control_a_selects_the_field_instead_of_typing() {
    let mut g = Greeter::new();
    g.type_text("abc");
    g.chord(CONTROL_L, 'a' as u32, Some("\u{1}"));
    g.type_text("x");
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("x".into())]);
}

#[test]
fn without_power_buttons_tab_never_reaches_power() {
    let mut g = Greeter::new();
    g.window.set_power_visible(false);
    g.window.set_user_name("alice");
    for _ in 0..7 {
        g.tap(TAB);
        g.tap(SPACE);
        g.tap(RETURN);
    }
    assert!(
        !g.take().iter().any(|sent| matches!(sent, Sent::Power(_))),
        "a hidden power row is not in the focus chain"
    );
}

#[test]
fn a_theme_without_focus_support_keeps_the_old_keys_and_never_powers_off() {
    let dir = std::env::temp_dir().join(format!("vigil-keyboard-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bare.slint");
    std::fs::write(&path, BARE_THEME).unwrap();
    let platform = VigilPlatform::install().expect("platform");
    vigil_theme::check(&path).expect("the bare theme meets the contract");
    let theme = Theme::load_or_default(Some(Path::new(&path)));
    let mut g = Greeter::with_theme(platform, &theme);
    for _ in 0..6 {
        g.tap(TAB);
        g.tap(SPACE);
        g.tap(RETURN);
    }
    g.type_text("pw");
    g.tap(RETURN);
    std::fs::remove_dir_all(&dir).unwrap();
    let sent = g.take();
    assert!(!sent.iter().any(|s| matches!(s, Sent::Power(_))));
    assert_eq!(sent.last(), Some(&Sent::Respond("pw".into())));
}

const BARE_THEME: &str = r#"
export component Bare inherits Window {
    in property <string> output-name;
    in property <bool> panel-visible;
    in property <image> background-source;
    in property <string> clock-text;
    in property <string> auth-state;
    in property <string> prompt-text;
    in property <bool> prompt-is-secret;
    in property <string> info-message;
    in property <string> error-message;
    in property <[string]> sessions;
    in property <int> selected-session;
    in property <bool> caps-lock;
    in property <string> status-banner;
    callback submit(string);
    callback cancel();
    callback session-changed(int);
    callback power-action(string);
    forward-focus: input;
    input := TextInput {
        accepted => { root.submit(self.text); self.text = ""; }
    }
    Rectangle {
        TouchArea { clicked => { root.power-action("poweroff"); } }
    }
}
"#;

const FIELD_Y: u32 = 419;
const TEXT_X: std::ops::Range<u32> = 500..735;

fn field_row(window: &mut OutputWindow, buf: &mut [u8]) -> Vec<(u8, u8, u8)> {
    vigil_ui::advance_timers();
    window.render_if_needed(FrameTarget {
        buffer: buf,
        width: W,
        height: H,
        stride: (W * 4) as usize,
        damage: None,
    });
    TEXT_X
        .step_by(2)
        .map(|x| {
            let i = ((FIELD_Y * W + x) * 4) as usize;
            (buf[i + 2], buf[i + 1], buf[i])
        })
        .collect()
}

#[test]
fn alt_f8_shows_and_hides_the_password() {
    let mut g = Greeter::new();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    g.type_text("aaaa");
    let masked = field_row(&mut g.window, &mut buf);
    g.chord(ALT_L, F8, None);
    let shown = field_row(&mut g.window, &mut buf);
    assert_ne!(masked, shown, "Alt+F8 shows the password");
    g.chord(ALT_L, F8, None);
    assert_eq!(
        masked,
        field_row(&mut g.window, &mut buf),
        "Alt+F8 again hides it"
    );

    g.tap(TAB);
    g.tap(SPACE);
    assert_ne!(
        masked,
        field_row(&mut g.window, &mut buf),
        "Space on show/hide shows it"
    );
    g.tap(ESCAPE);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("aaaa".into())]);
}

#[test]
fn walking_focus_never_moves_the_card() {
    let mut g = Greeter::with_pickers();
    g.window.set_clock("12:34");
    let mut buf = vec![0u8; (W * H * 4) as usize];
    let mut column = |g: &mut Greeter| {
        field_row(&mut g.window, &mut buf);
        (0..H)
            .step_by(2)
            .map(|y| {
                let i = ((y * W + 640) * 4) as usize;
                (buf[i + 2], buf[i + 1], buf[i])
            })
            .collect::<Vec<_>>()
    };
    let rest: Vec<_> = column(&mut g)
        .into_iter()
        .take((300 / 2) as usize)
        .collect();
    for stop in 0..6 {
        g.tap(TAB);
        let now = column(&mut g);
        assert_eq!(rest, now[..rest.len()], "the clock moved at stop {stop}");
    }
}

fn luminance(color: slint::Color) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(color.red()) + 0.7152 * lin(color.green()) + 0.0722 * lin(color.blue())
}

#[test]
fn the_focus_ring_tones_are_far_enough_apart_to_show_on_any_background() {
    let _platform = VigilPlatform::install().expect("platform");
    let component = Theme::load_or_default(None)
        .instantiate()
        .expect("instantiate");
    let color = |name| match component.get_global_property("Theme", name) {
        Ok(slint_interpreter::Value::Brush(brush)) => brush.color(),
        other => panic!("Theme.{name}: {other:?}"),
    };
    let (light, dark) = (luminance(color("fg")), luminance(color("page-bg")));
    let ratio = (light.max(dark) + 0.05) / (light.min(dark) + 0.05);
    assert!(
        ratio >= 9.0,
        "fg on page-bg is {ratio:.2}:1; at 9:1 or more any neighbour is 3:1 from one tone"
    );
}

#[test]
fn a_modifier_held_when_the_panel_leaves_does_not_stick() {
    let mut g = Greeter::new();
    g.event(CONTROL_L, None, true);
    g.window.set_panel_visible(false);
    g.window.set_panel_visible(true);
    g.window.show_prompt("Password:", true);
    g.type_text("pw");
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("pw".into())]);
}

#[test]
fn holding_an_arrow_steps_a_picker_once() {
    let mut g = Greeter::with_pickers();
    g.chord(SHIFT_L, ISO_LEFT_TAB, Some("\t"));
    for _ in 0..5 {
        g.event(RIGHT, None, true);
    }
    g.event(RIGHT, None, false);
    g.tap(RIGHT);
    assert_eq!(
        g.take(),
        [Sent::User(1), Sent::User(2)],
        "key repeat never cycles users, each of which may open a sign-in"
    );
}

#[test]
fn control_z_after_a_submit_neither_panics_nor_restores_the_password() {
    let mut g = Greeter::new();
    g.type_text("a");
    g.tap(RETURN);
    g.chord(CONTROL_L, 'z' as u32, Some("\u{1a}"));
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Respond("a".into()), Sent::Respond("".into())]
    );
}

#[test]
fn no_control_chord_edits_history_or_types_into_a_cleared_field() {
    let mut g = Greeter::new();
    for letter in 'a'..='z' {
        for shift in [false, true] {
            let keysym = if shift {
                letter.to_ascii_uppercase()
            } else {
                letter
            } as u32;
            let control = char::from(letter as u8 - b'a' + 1).to_string();
            let bare = char::from_u32(keysym).unwrap().to_string();
            for utf8 in [control.as_str(), bare.as_str()] {
                g.type_text("pw");
                g.tap(RETURN);
                if shift {
                    g.event(SHIFT_L, None, true);
                }
                g.chord(CONTROL_L, keysym, Some(utf8));
                if shift {
                    g.event(SHIFT_L, None, false);
                }
                g.type_text("x");
                g.tap(RETURN);
                assert_eq!(
                    g.take(),
                    [Sent::Respond("pw".into()), Sent::Respond("x".into())],
                    "Ctrl+{}{letter} sent as {utf8:?}",
                    if shift { "Shift+" } else { "" }
                );
            }
        }
    }
}

#[test]
fn a_composed_character_types_once() {
    const MULTI_KEY: u32 = 0xff20;
    let mut g = Greeter::new();
    g.event(MULTI_KEY, None, true);
    g.event(MULTI_KEY, None, false);
    g.event('a' as u32, None, true);
    g.event('a' as u32, None, false);
    g.event('\'' as u32, Some("á"), true);
    g.event('\'' as u32, None, false);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("á".into())]);
}

#[test]
fn control_z_never_brings_back_a_deleted_password() {
    const BACKSPACE: u32 = 0xff08;
    let mut g = Greeter::new();
    g.type_text("secret");
    for _ in 0..6 {
        g.tap(BACKSPACE);
    }
    g.chord(CONTROL_L, 'z' as u32, Some("\u{1a}"));
    g.chord(ALT_L, F8, None);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("".into())]);
}

#[test]
fn a_held_enter_or_keypad_enter_submits_once() {
    const KP_ENTER: u32 = 0xff8d;
    let mut g = Greeter::new();
    for enter in [RETURN, KP_ENTER] {
        g.type_text("pw");
        for _ in 0..5 {
            g.event(enter, Some("\r"), true);
        }
        g.event(enter, None, false);
        assert_eq!(g.take(), [Sent::Respond("pw".into())], "{enter:#x}");
    }
}

#[test]
fn holding_alt_f8_shows_the_password_once() {
    let mut g = Greeter::new();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    g.type_text("aaaa");
    let masked = field_row(&mut g.window, &mut buf);
    g.event(ALT_L, None, true);
    for _ in 0..4 {
        g.event(F8, None, true);
    }
    g.event(F8, None, false);
    g.event(ALT_L, None, false);
    assert_ne!(
        masked,
        field_row(&mut g.window, &mut buf),
        "key repeat on Alt+F8 toggled the password back to hidden"
    );
}

#[test]
fn tab_then_enter_submits_instead_of_showing_the_password() {
    let mut g = Greeter::new();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    g.type_text("aaaa");
    let masked = field_row(&mut g.window, &mut buf);
    g.tap(TAB);
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("aaaa".into())]);
    g.type_text("aaaa");
    assert_eq!(masked, field_row(&mut g.window, &mut buf), "still hidden");
}

fn settled_row(g: &mut Greeter, buf: &mut [u8]) -> Vec<(u8, u8, u8)> {
    std::thread::sleep(std::time::Duration::from_millis(400));
    field_row(&mut g.window, buf)
}

fn a_shown_password_hides_again_after(event: impl FnOnce(&mut Greeter)) {
    let mut g = Greeter::new();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    g.type_text("aaaa");
    let masked = settled_row(&mut g, &mut buf);
    g.chord(ALT_L, F8, None);
    assert_ne!(
        masked,
        field_row(&mut g.window, &mut buf),
        "Alt+F8 shows it"
    );
    event(&mut g);
    assert_eq!(masked, settled_row(&mut g, &mut buf));
}

#[test]
fn a_shown_password_hides_again_after_a_submit() {
    a_shown_password_hides_again_after(|g| {
        g.tap(RETURN);
        g.type_text("aaaa");
    });
}

#[test]
fn a_shown_password_hides_again_after_a_failure() {
    a_shown_password_hides_again_after(|g| g.window.show_error("Authentication failure"));
}

#[test]
fn a_shown_password_hides_again_after_a_panel_move() {
    a_shown_password_hides_again_after(|g| {
        g.window.set_panel_visible(false);
        g.window.set_panel_visible(true);
    });
}

#[test]
fn a_shown_password_hides_again_after_idle_but_not_while_typing() {
    let idle = std::time::Duration::from_millis(300);
    let mut g = Greeter::new();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    g.window.set_reveal_idle(idle);
    g.type_text("aaaa");
    let masked = settled_row(&mut g, &mut buf);
    g.chord(ALT_L, F8, None);
    for _ in 0..3 {
        std::thread::sleep(idle / 2);
        g.type_text("a");
        g.tap(0xff08);
    }
    assert_ne!(
        masked,
        field_row(&mut g.window, &mut buf),
        "typing keeps it shown"
    );
    std::thread::sleep(idle * 2);
    assert_eq!(masked, field_row(&mut g.window, &mut buf), "idle hides it");
}

fn click(g: &mut Greeter, x: f64, y: f64) {
    g.window.dispatch(InputEvent::PointerAbsolute { x, y });
    for pressed in [true, false] {
        g.window.dispatch(InputEvent::PointerButton {
            button: 0x110,
            pressed,
        });
    }
    g.settle();
}

fn a_double_click_on_power_never_acts(width: u32, height: u32) {
    let platform = VigilPlatform::install().expect("platform");
    let mut g = Greeter::sized(platform, &Theme::load_or_default(None), width, height);
    let y = f64::from(height - 28 - 36 / 2);
    for x in (width / 2..width - 28).step_by(4) {
        click(&mut g, f64::from(x), y);
        click(&mut g, f64::from(x), y);
        g.tap(ESCAPE);
        let sent = g.take();
        assert!(
            !sent.iter().any(|s| matches!(s, Sent::Power(_))),
            "a double click at x {x} of {width}x{height} sent {sent:?}"
        );
    }

    g.tap(ESCAPE);
    for _ in 0..3 {
        g.tap(TAB);
    }
    g.tap(RETURN);
    std::thread::sleep(std::time::Duration::from_millis(700));
    vigil_ui::advance_timers();
    let sent = (width / 2..width - 28).rev().step_by(4).find_map(|x| {
        click(&mut g, f64::from(x), y);
        g.take().pop()
    });
    assert_eq!(
        sent,
        Some(Sent::Power(PowerAction::Poweroff)),
        "a deliberate click on an open confirm still shuts down"
    );
}

#[test]
fn a_double_click_on_power_never_acts_in_landscape() {
    a_double_click_on_power_never_acts(W, H);
}

#[test]
fn a_double_click_on_power_never_acts_in_portrait() {
    a_double_click_on_power_never_acts(H, W);
}

fn frame(window: &mut OutputWindow) -> Vec<u8> {
    let mut buf = vec![0u8; (W * H * 4) as usize];
    vigil_ui::advance_timers();
    window.request_present();
    window.render_if_needed(FrameTarget {
        buffer: &mut buf,
        width: W,
        height: H,
        stride: (W * 4) as usize,
        damage: None,
    });
    buf
}

fn ring_contrast(g: &mut Greeter) -> f64 {
    let before = frame(&mut g.window);
    g.tap(TAB);
    let after = frame(&mut g.window);
    g.tap(ESCAPE);
    let (mut light, mut dark) = (0.0f64, 1.0f64);
    for (old, new) in before.chunks(4).zip(after.chunks(4)) {
        if old != new {
            let l = luminance(slint::Color::from_rgb_u8(new[2], new[1], new[0]));
            light = light.max(l);
            dark = dark.min(l);
        }
    }
    (light + 0.05) / (dark.min(light) + 0.05)
}

#[test]
fn a_kit_that_flattens_fg_and_page_bg_still_gets_a_9_to_1_ring() {
    let mut g = Greeter::new();
    let ratio = ring_contrast(&mut g);
    assert!(ratio >= 9.0, "the built-in ring's tones are {ratio:.2}:1");
    g.window.apply_kit_tokens(&slint_kit::TokenSet {
        mode: "dark".into(),
        colors: Default::default(),
    });
    let ratio = ring_contrast(&mut g);
    assert!(
        ratio >= 9.0,
        "under the kit the ring's tones are {ratio:.2}:1"
    );
}

fn a_pam_message_stays_shown_while_navigating(show: impl FnOnce(&mut OutputWindow)) {
    let mut g = Greeter::with_pickers();
    let plain = frame(&mut g.window);
    g.window.show_info("Touch your security key");
    let info = frame(&mut g.window);
    let rows: Vec<usize> = (0..H as usize)
        .filter(|&y| {
            let row = y * W as usize * 4..(y + 1) * W as usize * 4;
            plain[row.clone()] != info[row]
        })
        .collect();
    assert!(!rows.is_empty(), "the message line is found");
    show(&mut g.window);
    frame(&mut g.window);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let shown = frame(&mut g.window);
    for stop in 0..5 {
        g.tap(TAB);
        let now = frame(&mut g.window);
        for &y in &rows {
            let row = y * W as usize * 4..(y + 1) * W as usize * 4;
            assert!(
                shown[row.clone()] == now[row],
                "the message line changed at row {y} with focus on stop {stop}"
            );
        }
    }
}

#[test]
fn a_pam_info_message_stays_shown_while_navigating() {
    a_pam_message_stays_shown_while_navigating(|_| {});
}

#[test]
fn a_pam_error_stays_shown_while_navigating() {
    a_pam_message_stays_shown_while_navigating(|w| w.show_error("Authentication failure"));
}

#[test]
fn a_retry_typed_during_the_check_survives_the_failure_hidden() {
    let mut g = Greeter::new();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    g.type_text("aaaa");
    let masked = settled_row(&mut g, &mut buf);
    g.tap(RETURN);
    g.window.set_busy(true);
    g.type_text("aa");
    g.chord(ALT_L, F8, None);
    g.window.set_busy(false);
    g.window.show_error("Authentication failure");
    g.type_text("aa");
    assert_eq!(
        masked,
        settled_row(&mut g, &mut buf),
        "hidden after the failure"
    );
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Respond("aaaa".into()), Sent::Respond("aaaa".into())]
    );
}

#[test]
fn tab_with_both_ctrl_keys_down_never_replaces_the_field() {
    let mut g = Greeter::new();
    g.type_text("secret");
    g.event(CONTROL_L, None, true);
    g.event(CONTROL_R, None, true);
    g.event(CONTROL_R, None, false);
    g.event(TAB, Some("\t"), true);
    g.event(TAB, Some("\t"), false);
    g.event(CONTROL_L, None, false);
    g.type_text("x");
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("secretx".into())]);
}

#[test]
fn ctrl_a_with_one_of_two_ctrl_keys_released_types_no_letter() {
    let mut g = Greeter::new();
    g.type_text("secret");
    g.event(CONTROL_L, None, true);
    g.event(CONTROL_R, None, true);
    g.event(CONTROL_R, None, false);
    g.event('a' as u32, Some("a"), true);
    g.event('a' as u32, Some("a"), false);
    g.event(CONTROL_L, None, false);
    g.tap(RETURN);
    assert_eq!(
        g.take(),
        [Sent::Respond("secret".into())],
        "Ctrl+A selects, so no letter reaches the password"
    );
}

fn tab_under_a_modifier_never_reaches_the_field(modifier: u32) {
    let mut g = Greeter::new();
    g.type_text("secret");
    g.chord(modifier, TAB, Some("\t"));
    g.type_text("x");
    g.tap(RETURN);
    assert_eq!(g.take(), [Sent::Respond("secretx".into())]);
}

#[test]
fn ctrl_tab_never_reaches_the_field() {
    tab_under_a_modifier_never_reaches_the_field(CONTROL_L);
}

#[test]
fn alt_tab_never_reaches_the_field() {
    tab_under_a_modifier_never_reaches_the_field(ALT_L);
}
