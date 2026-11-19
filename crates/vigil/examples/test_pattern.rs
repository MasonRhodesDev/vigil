use std::fs::OpenOptions;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;

use vigil_core::{OutputEvent, Presenter};
use vigil_outputs::OutputManager;
use vigil_present_dumb::DumbBufferPresenter;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| usage());
    let hold_secs: u64 = args
        .next()
        .map(|s| s.parse().expect("seconds"))
        .unwrap_or(3);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc_o_cloexec())
        .open(&path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    let fd: OwnedFd = file.into();

    let (mut outputs, _notifier) = OutputManager::new(fd, 0).expect("drm device");
    let events = outputs.scan().expect("connector scan");

    let mut presenters = Vec::new();
    for event in events {
        if let OutputEvent::Added(id, info) = event {
            vigil_log::out!(
                "output {}: {}x{}@{}mHz",
                info.connector,
                info.width,
                info.height,
                info.refresh_mhz
            );
            let surface = outputs.create_surface(id).expect("surface");
            presenters.push(DumbBufferPresenter::new(surface).expect("presenter"));
        }
    }
    assert!(!presenters.is_empty(), "no connected outputs on {path}");

    for p in presenters.iter_mut() {
        p.with_frame(&mut |canvas| {
            let vigil_core::Canvas::Cpu(t) = canvas else {
                panic!("dumb presenter handed a GL canvas");
            };
            gradient(t, 0);
            true
        })
        .expect("frame 1");
    }
    vigil_log::out!(
        "frame 1 committed (modeset) on {} output(s)",
        presenters.len()
    );
    std::thread::sleep(std::time::Duration::from_secs(hold_secs));

    for p in presenters.iter_mut() {
        p.with_frame(&mut |canvas| {
            let vigil_core::Canvas::Cpu(t) = canvas else {
                panic!("dumb presenter handed a GL canvas");
            };
            gradient(t, 128);
            true
        })
        .expect("frame 2");
    }
    vigil_log::out!("frame 2 flipped");
    std::thread::sleep(std::time::Duration::from_secs(hold_secs));

    vigil_log::out!("PASS test_pattern");
}

fn gradient(t: vigil_core::FrameTarget<'_>, phase: u32) {
    for y in 0..t.height as usize {
        let row = &mut t.buffer[y * t.stride..y * t.stride + t.width as usize * 4];
        for x in 0..t.width as usize {
            let r = ((x as u32 * 255 / t.width) + phase) & 0xff;
            let g = ((y as u32 * 255 / t.height) + phase) & 0xff;
            let b = 0x40u32;
            row[x * 4..x * 4 + 4].copy_from_slice(&(b | (g << 8) | (r << 16)).to_le_bytes());
        }
    }
}

fn libc_o_cloexec() -> i32 {
    0o2000000
}

fn usage() -> ! {
    vigil_log::say!("usage: test_pattern </dev/dri/cardN> [hold-seconds]");
    std::process::exit(2);
}
