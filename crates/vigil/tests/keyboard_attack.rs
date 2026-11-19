#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::rc::Rc;
use std::time::Instant;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};
use vigil_core::{AuthUi, InputEvent, KeyText, OutputId, UiMessage};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const TAB: u32 = 0xff09;
const ISO_LEFT_TAB: u32 = 0xfe20;
const RETURN: u32 = 0xff0d;
const KP_ENTER: u32 = 0xff8d;
const ESCAPE: u32 = 0xff1b;
const LEFT: u32 = 0xff51;
const RIGHT: u32 = 0xff53;
const SPACE: u32 = 0x20;

const KEYS: [(u32, Option<&str>); 24] = [
    (TAB, Some("\t")),
    (ISO_LEFT_TAB, None),
    (RETURN, Some("\r")),
    (KP_ENTER, Some("\r")),
    (ESCAPE, Some("\u{1b}")),
    (LEFT, None),
    (RIGHT, None),
    (0xff52, None),
    (0xff54, None),
    (SPACE, Some(" ")),
    (0xffc5, None),
    (0xff08, Some("\u{8}")),
    (0xffff, Some("\u{7f}")),
    (0xffe1, None),
    (0xffe2, None),
    (0xffe3, None),
    (0xffe4, None),
    (0xffe9, None),
    (0xffea, None),
    (0x61, Some("a")),
    (0x76, Some("v")),
    (0x7a, Some("z")),
    (0xe9, Some("\u{e9}")),
    (0xfe03, None),
];

#[derive(Debug, Clone)]
enum Op {
    Key {
        keysym: u32,
        utf8: Option<String>,
        pressed: bool,
    },
    Prompt(bool),
    Busy(bool),
    Failure,
    Reset,
    Panel(bool),
    Power(bool),
    Pickers(bool),
}

fn key() -> impl Strategy<Value = Op> {
    let utf8 = prop_oneof![
        8 => Just(None::<String>),
        1 => "[\\x00-\\x7f\u{e9}\u{4e2d}\u{f702}\u{1f600}]{0,4}".prop_map(Some),
    ];
    (0..KEYS.len(), utf8, prop::bool::weighted(0.7)).prop_map(|(at, hostile, pressed)| {
        let (keysym, utf8) = KEYS[at];
        Op::Key {
            keysym,
            utf8: hostile.or(utf8.map(str::to_owned)),
            pressed,
        }
    })
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        40 => key(),
        1 => any::<bool>().prop_map(Op::Prompt),
        1 => any::<bool>().prop_map(Op::Busy),
        1 => Just(Op::Failure),
        1 => Just(Op::Reset),
        1 => any::<bool>().prop_map(Op::Panel),
        1 => any::<bool>().prop_map(Op::Power),
        1 => any::<bool>().prop_map(Op::Pickers),
    ]
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(), 0..160)
}

fn activates(keysym: u32) -> bool {
    matches!(keysym, RETURN | KP_ENTER | SPACE)
}

fn moves(keysym: u32) -> bool {
    matches!(keysym, TAB | ISO_LEFT_TAB | LEFT | RIGHT)
}

// What the keyboard alone shows: a press repeats the last key pressed, not
// counting modifiers, when nothing released it since. Every rule here is about
// the keys, so it holds whatever the focus code does inside.
#[derive(Default)]
struct Oracle {
    last_press: Option<u32>,
    panel: bool,
    power: bool,
    armed: u8,
}

impl Oracle {
    fn fresh(&self, keysym: u32) -> bool {
        self.last_press != Some(keysym)
    }

    fn key(&mut self, keysym: u32, pressed: bool) -> Option<bool> {
        if !pressed {
            if self.last_press == Some(keysym) {
                self.last_press = None;
            }
            return None;
        }
        let fresh = self.fresh(keysym);
        if !vigil_core::MODIFIER_KEYSYMS.contains(&keysym) {
            self.last_press = Some(keysym);
        }
        Some(fresh)
    }

    fn after_key(&mut self, keysym: u32, fresh: bool) {
        self.armed = match self.armed {
            _ if keysym == ESCAPE && fresh => 0,
            0 if activates(keysym) && fresh => 1,
            1 if moves(keysym) => 2,
            armed => armed,
        };
    }
}

struct Lock {
    window: OutputWindow,
    log: Rc<RefCell<Vec<UiMessage>>>,
}

impl Lock {
    fn open(platform: &VigilPlatform, theme: &Theme) -> Self {
        let component = theme.instantiate().expect("instantiate");
        let adapter = platform.claim_last_adapter().expect("adapter");
        let window =
            OutputWindow::new(OutputId(1), 1280, 800, 1.0, adapter, component).expect("window");
        let log: Rc<RefCell<Vec<UiMessage>>> = Rc::default();
        let sink = log.clone();
        window.on_ui_message(Rc::new(move |message| sink.borrow_mut().push(message)));
        let mut lock = Self { window, log };
        lock.window.set_panel_visible(true);
        lock.window.set_power_visible(true);
        lock.window.show_prompt("Password:", true);
        lock
    }

    fn sent(&self) -> Vec<UiMessage> {
        self.log.borrow_mut().drain(..).collect()
    }
}

fn check_answer(on: Option<(u32, bool)>) -> Result<(), String> {
    match on {
        Some((RETURN | KP_ENTER, true)) => Ok(()),
        Some((keysym, fresh)) => Err(format!(
            "an answer went out on keysym {keysym:#x} (fresh: {fresh}), not a fresh Enter"
        )),
        None => Err("an answer went out with no key pressed".into()),
    }
}

fn check_power(oracle: &Oracle, on: Option<(u32, bool)>) -> Result<(), String> {
    if !oracle.panel || !oracle.power {
        return Err(format!(
            "a power action fired with the panel shown {} and power shown {}",
            oracle.panel, oracle.power
        ));
    }
    match on {
        Some((keysym, true)) if activates(keysym) => {}
        other => {
            return Err(format!(
                "a power action fired on {other:x?}, not a fresh Enter or Space"
            ));
        }
    }
    if oracle.armed < 2 {
        return Err(format!(
            "a power action fired without an arming press and a move to Confirm since the last Escape or action (progress {})",
            oracle.armed
        ));
    }
    Ok(())
}

fn drive(platform: &VigilPlatform, theme: &Theme, ops: &[Op]) -> Result<(), String> {
    let mut lock = Lock::open(platform, theme);
    let mut oracle = Oracle {
        panel: true,
        power: true,
        ..Oracle::default()
    };
    for (at, op) in ops.iter().enumerate() {
        let mut on = None;
        match op {
            Op::Key {
                keysym,
                utf8,
                pressed,
            } => {
                on = oracle.key(*keysym, *pressed).map(|fresh| (*keysym, fresh));
                lock.window.dispatch(InputEvent::Key {
                    keysym: *keysym,
                    utf8: utf8.clone().map(KeyText::from),
                    pressed: *pressed,
                });
            }
            Op::Prompt(secret) => lock.window.show_prompt("Password:", *secret),
            Op::Busy(busy) => lock.window.set_busy(*busy),
            Op::Failure => lock.window.show_error("Authentication failure"),
            Op::Reset => lock.window.reset_input(),
            Op::Panel(shown) => {
                lock.window.set_panel_visible(*shown);
                oracle.panel = *shown;
                if !shown {
                    oracle.armed = 0;
                    oracle.last_press = None;
                }
            }
            Op::Power(shown) => {
                lock.window.set_power_visible(*shown);
                oracle.power = *shown;
                if !shown {
                    oracle.armed = 0;
                }
            }
            Op::Pickers(shown) => {
                let names: &[String] = if *shown {
                    &["alice".into(), "bob".into()]
                } else {
                    &[]
                };
                lock.window.set_users(names);
                lock.window.set_sessions(names);
            }
        }
        for message in lock.sent() {
            let result = match &message {
                UiMessage::Respond(_) => check_answer(on),
                UiMessage::Power(_) => check_power(&oracle, on),
                UiMessage::Cancel | UiMessage::SelectSession(_) | UiMessage::SelectUser(_) => {
                    Ok(())
                }
            };
            result.map_err(|why| format!("step {at} ({op:?}): {why}"))?;
            if matches!(message, UiMessage::Power(_)) {
                oracle.armed = 0;
            }
        }
        if let Some((keysym, fresh)) = on {
            oracle.after_key(keysym, fresh);
        }
    }
    Ok(())
}

fn tap(keysym: u32) -> [Op; 2] {
    let utf8 = KEYS
        .iter()
        .find(|(key, _)| *key == keysym)
        .and_then(|(_, utf8)| utf8.map(str::to_owned));
    [true, false].map(|pressed| Op::Key {
        keysym,
        utf8: utf8.clone(),
        pressed,
    })
}

fn hold(keysym: u32, times: usize) -> Vec<Op> {
    let press = tap(keysym)[0].clone();
    vec![press; times]
}

fn pinned() -> Vec<(&'static str, Vec<Op>)> {
    let walk = |tabs: usize| (0..tabs).flat_map(|_| tap(TAB)).collect::<Vec<_>>();
    vec![
        ("a held Enter on a power button never confirms", {
            let mut ops = walk(3);
            ops.extend(hold(RETURN, 6));
            ops
        }),
        (
            "Enter, Enter, Enter on a power button only arms and backs out",
            {
                let mut ops = walk(3);
                (0..3).for_each(|_| ops.extend(tap(RETURN)));
                ops
            },
        ),
        (
            "Space then a held Tab then Escape then Enter never confirms",
            {
                let mut ops = walk(3);
                ops.extend(tap(SPACE));
                ops.extend(hold(TAB, 4));
                ops.extend(tap(ESCAPE));
                ops.extend(tap(RETURN));
                ops
            },
        ),
        (
            "a key whose text starts with a newline never submits",
            vec![Op::Key {
                keysym: 0x61,
                utf8: Some("\na".into()),
                pressed: true,
            }],
        ),
        ("a hidden then shown power row drops the arm", {
            let mut ops = walk(3);
            ops.extend(tap(RETURN));
            ops.push(Op::Power(false));
            ops.push(Op::Power(true));
            ops.extend(tap(RIGHT));
            ops.extend(tap(RETURN));
            ops
        }),
    ]
}

fn runner(cases: u32, seed: u64) -> TestRunner {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    TestRunner::new_with_rng(
        Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &bytes),
    )
}

#[test]
fn hostile_keys_never_power_off_or_answer_without_a_fresh_enter() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    let mut held = true;
    for (name, ops) in pinned() {
        match drive(&platform, &theme, &ops) {
            Ok(()) => vigil_log::out!("keyboard attack: pinned: {name}: held"),
            Err(why) => {
                vigil_log::out!("keyboard attack: pinned: {name}: FAILED\n{why}");
                held = false;
            }
        }
    }
    let cases = std::env::var("VIGIL_KEYBOARD_ATTACK_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(400);
    let seed = match std::env::var("VIGIL_KEYBOARD_ATTACK_SEED") {
        Ok(seed) => seed
            .parse()
            .unwrap_or_else(|_| panic!("VIGIL_KEYBOARD_ATTACK_SEED is not a u64: {seed}")),
        Err(_) => RandomState::new().hash_one(()),
    };
    let started = Instant::now();
    let outcome = runner(cases, seed).run(&ops(), |ops| {
        drive(&platform, &theme, &ops).map_err(TestCaseError::fail)
    });
    match outcome {
        Ok(()) => vigil_log::out!(
            "keyboard attack: {cases} cases held in {:.1?}; seed {seed}",
            started.elapsed()
        ),
        Err(failure) => {
            vigil_log::out!(
                "keyboard attack: FAILED with VIGIL_KEYBOARD_ATTACK_SEED={seed}\n{failure}"
            );
            held = false;
        }
    }
    assert!(held, "a keyboard attack got through");
}
