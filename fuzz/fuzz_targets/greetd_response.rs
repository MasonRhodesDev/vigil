#![no_main]

use greetd_ipc::Response;
use greetd_ipc::codec::SyncCodec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = Response::read_from(&mut std::io::Cursor::new(data));
});
