use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{PipeReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub enum Stderr {
    DevFull,
    ReaderlessPipe,
    StalledPipe,
}

pub fn exit_code(binary: &str, args: &[impl AsRef<OsStr>], stderr: Stderr) -> Option<i32> {
    let mut _unread: Option<PipeReader> = None;
    let target: Stdio = match stderr {
        Stderr::DevFull => OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("open /dev/full")
            .into(),
        Stderr::ReaderlessPipe => std::io::pipe().expect("pipe").1.into(),
        Stderr::StalledPipe => {
            let (reader, writer) = std::io::pipe().expect("pipe");
            fill(&writer);
            _unread = Some(reader);
            writer.into()
        }
    };
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(target)
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("wait") {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

pub fn fill(writer: &impl AsRawFd) {
    let mut own: File = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(format!("/proc/self/fd/{}", writer.as_raw_fd()))
        .expect("second description of the pipe");
    for chunk in [&[b'x'; 4096][..], b"x"] {
        while own.write(chunk).is_ok() {}
    }
}
