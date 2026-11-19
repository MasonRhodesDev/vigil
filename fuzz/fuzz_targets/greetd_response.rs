#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = vigil_auth::read_response(&mut std::io::Cursor::new(data));
});
