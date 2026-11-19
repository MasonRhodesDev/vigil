#![no_main]

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use vigil_core::{InputEvent, KeymapSettings};
use vigil_input::fuzzing::Keyboard;

const LAYOUTS: [(&str, &str); 4] = [("us", ""), ("us", "intl"), ("de", ""), ("ru", "")];
const LOCALES: [&str; 2] = ["en_US.UTF-8", "C"];

// Compiling a keymap costs far more than a run, so each is built once.
thread_local! {
    static KEYBOARDS: RefCell<[[Option<Keyboard>; 2]; 4]> = const { RefCell::new([const { [const { None }; 2] }; 4]) };
}

fn typed(event: Option<InputEvent>) {
    if let Some(InputEvent::Key { utf8, .. }) = event {
        assert_ne!(
            utf8.as_ref().map(|text| text.as_str()),
            Some(""),
            "a key carried empty text"
        );
    }
}

// Byte 0 picks the layout and the compose locale. Each later triple is a
// key code, then a press, a release or a wait in milliseconds.
fuzz_target!(|data: &[u8]| {
    let Some((&pick, steps)) = data.split_first() else {
        return;
    };
    let layout = usize::from(pick) % LAYOUTS.len();
    let locale = usize::from(pick >> 4) % LOCALES.len();
    KEYBOARDS.with_borrow_mut(|keyboards| {
        let keyboard = keyboards[layout][locale].get_or_insert_with(|| {
            let settings = KeymapSettings {
                layout: LAYOUTS[layout].0.into(),
                variant: LAYOUTS[layout].1.into(),
                ..KeymapSettings::default()
            };
            Keyboard::new(&settings, LOCALES[locale]).expect("compile the keymap")
        });
        keyboard.reset();
        run(keyboard, steps);
    });
});

fn keysym(event: &Option<InputEvent>) -> Option<u32> {
    match event {
        Some(InputEvent::Key { keysym, .. }) => Some(*keysym),
        _ => None,
    }
}

// A release reports the keysym its press reported, whatever the layout or
// level did in between, and Return, keypad Enter and Escape never repeat.
fn run(keyboard: &mut Keyboard, steps: &[u8]) {
    let mut held: HashMap<u16, u32> = HashMap::new();
    for step in steps.chunks_exact(3) {
        let code = u16::from_le_bytes([step[0], step[1]]);
        match step[2] % 3 {
            0 => {
                let event = keyboard.key(code, true);
                match keysym(&event) {
                    Some(keysym) => held.insert(code, keysym),
                    None => held.remove(&code),
                };
                typed(event);
            }
            1 => {
                let event = keyboard.key(code, false);
                if let (Some(pressed), Some(released)) = (held.remove(&code), keysym(&event)) {
                    assert_eq!(
                        released, pressed,
                        "code {code} was pressed as {pressed:#x} and released as {released:#x}"
                    );
                }
                typed(event);
            }
            _ => {
                let wait = Duration::from_millis(u64::from(code));
                let repeats = keyboard.wait(wait);
                assert!(
                    repeats.len() <= wait.as_millis() as usize / 33 + 1,
                    "{} repeats in {wait:?}",
                    repeats.len()
                );
                for event in repeats {
                    if let InputEvent::Key { keysym, .. } = &event {
                        assert!(
                            !vigil_core::ONE_SHOT_KEYSYMS.contains(keysym),
                            "{keysym:#x} repeated"
                        );
                    }
                    typed(Some(event));
                }
            }
        }
        let _ = keyboard.caps_lock();
    }
}
