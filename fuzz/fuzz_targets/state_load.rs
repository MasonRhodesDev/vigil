#![no_main]

use std::path::PathBuf;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use vigil_config::State;

static FILE: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::temp_dir().join(format!("vigil-fuzz-state-{}.toml", std::process::id()))
});

fuzz_target!(|data: &[u8]| {
    std::fs::write(&*FILE, data).expect("write the state file");
    let Some(state) = State::load(&FILE) else {
        return;
    };
    assert!(state.store(&FILE), "a loaded state could not be stored");
    assert_eq!(
        State::load(&FILE).as_ref(),
        Some(&state),
        "a stored state did not load back as it was"
    );
});
