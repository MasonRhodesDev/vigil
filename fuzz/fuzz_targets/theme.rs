#![no_main]

use std::path::PathBuf;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;

static FILE: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::temp_dir().join(format!("vigil-fuzz-theme-{}.slint", std::process::id()))
});

fuzz_target!(|data: &[u8]| {
    std::fs::write(&*FILE, data).expect("write the theme file");
    let _ = vigil_theme::check(&FILE);
});
