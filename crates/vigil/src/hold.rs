//! The exit holder: keep the last frame on screen between vigil's exit and
//! the next owner's first frame.
//!
//! When vigil exits, the kernel closes its DRM file, which removes the
//! framebuffers the file made, and removing a framebuffer that a CRTC is
//! scanning out blanks that CRTC. The screens then stay black for as long as
//! greetd takes to start the session. The fix is to keep the DRM file open:
//! a forked child inherits the same open file (not a new one), so the
//! framebuffers, and the picture, survive the parent.
//!
//! What the child must get right:
//!
//! - DRM master. The file is master (libseat handed it over that way) and
//!   the next owner needs master on a fresh file. The parent drops master on
//!   the shared file before forking, so the session is never refused. logind
//!   and seatd drop it on the same file when the greeter's seat is released;
//!   dropping it first only removes the dependence on their timing. If the
//!   drop fails for any reason except "this file is not master", nothing is
//!   held and vigil exits as before.
//! - When to stop. Poll `GETCRTC` on each held CRTC. The next owner has taken
//!   over a screen once its framebuffer id is no longer the one vigil left (or
//!   is 0, the screen was turned off). Every held screen must have changed, or
//!   the child would close the file and blank a screen still showing the rest
//!   frame. It also stops after a timeout, and when the device errors.
//!   Closing the file after the new owner has presented removes only vigil's
//!   own, no longer scanned-out framebuffers, which blanks nothing.
//! - Not being in the way. The child closes every fd but the DRM fds (so it
//!   holds no greetd socket or seat connection), points stdin and stdout at
//!   /dev/null, and starts a new session so a signal to the greeter's
//!   process group misses it. It never signals or waits on anyone, and
//!   nothing waits on it.
//!
//! The child runs after `fork` in a process that has threads, so it uses
//! only raw libc calls and a stack buffer: no allocation, no locks, no
//! `eprintln!`. It logs to stderr, which stays open (the journal).

use std::fmt::Write as _;
use std::os::fd::RawFd;
use std::time::Duration;

/// One DRM device to hold: the raw fd and `(crtc id, framebuffer id)` for
/// every CRTC that is scanning vigil's frame out.
#[derive(Debug, Clone)]
pub struct Device {
    pub fd: RawFd,
    pub crtcs: Vec<(u32, u32)>,
}

const POLL_MS: u32 = 20;

// drm.h: DRM_IOCTL_DROP_MASTER = _IO('d', 0x1f);
// DRM_IOCTL_MODE_GETCRTC = _IOWR('d', 0xA1, struct drm_mode_crtc).
const DROP_MASTER: libc::c_ulong = 0x641f;
const GETCRTC: libc::c_ulong = (3 << 30) | (104 << 16) | (0x64 << 8) | 0xA1;

/// `struct drm_mode_crtc`, 104 bytes.
#[repr(C)]
struct ModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: [u8; 68],
}

/// Current framebuffer id on a CRTC (0: none), or the errno.
fn crtc_fb(fd: RawFd, crtc: u32) -> Result<u32, i32> {
    let mut c = ModeCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: crtc,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    // SAFETY: GETCRTC reads and writes one struct drm_mode_crtc, which `c`
    // is laid out as; count_connectors is 0, so no pointer is followed.
    let r = unsafe { libc::ioctl(fd, GETCRTC, &mut c as *mut ModeCrtc) };
    if r < 0 { Err(errno()) } else { Ok(c.fb_id) }
}

fn errno() -> i32 {
    // SAFETY: errno location is thread-local and always valid.
    unsafe { *libc::__errno_location() }
}

/// Drop DRM master on a file. Ok when the file is not master (EINVAL): there
/// is nothing to hand over then.
fn drop_master(fd: RawFd) -> Result<bool, i32> {
    // SAFETY: argument-less ioctl on a descriptor the caller owns.
    let r = unsafe { libc::ioctl(fd, DROP_MASTER) };
    match (r, errno()) {
        (0, _) => Ok(true),
        (_, libc::EINVAL) => Ok(false),
        (_, e) => Err(e),
    }
}

fn now_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

/// A fixed buffer that implements `fmt::Write`, flushed to stderr with one
/// `write(2)`: logging that is safe after `fork`.
struct Line {
    buf: [u8; 240],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Line {
            buf: [0; 240],
            len: 0,
        }
    }
    fn emit(mut self) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b'\n';
            self.len += 1;
        }
        // SAFETY: writes the initialised part of our own buffer.
        unsafe { libc::write(2, self.buf.as_ptr().cast(), self.len) };
    }
}

impl std::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = self.buf.len() - 1 - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

macro_rules! say {
    ($($arg:tt)*) => {{
        let mut l = Line::new();
        let _ = write!(l, $($arg)*);
        l.emit();
    }};
}

/// Why the child stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Released {
    HandedOver,
    TimedOut,
    DeviceError(i32),
}

/// One poll of every held CRTC: `Some(done)` when all changed, or the errno.
/// `left` tracks the CRTCs still showing vigil's frame.
fn poll_once(
    devices: &[Device],
    left: &mut [bool],
    read: &mut dyn FnMut(RawFd, u32) -> Result<u32, i32>,
    mut changed: impl FnMut(RawFd, u32, u32, u32),
) -> Result<bool, i32> {
    let mut i = 0;
    let mut all = true;
    for dev in devices {
        for &(crtc, fb) in &dev.crtcs {
            if left[i] {
                let now = read(dev.fd, crtc)?;
                if now != fb {
                    left[i] = false;
                    changed(dev.fd, crtc, fb, now);
                } else {
                    all = false;
                }
            }
            i += 1;
        }
    }
    Ok(all)
}

fn watch(devices: &[Device], timeout: Duration) -> Released {
    let start = now_ms();
    let total: usize = devices.iter().map(|d| d.crtcs.len()).sum();
    let mut left = [true; 64];
    let left = &mut left[..total.min(64)];
    loop {
        let r = poll_once(devices, left, &mut crtc_fb, |_, crtc, fb, now| {
            say!(
                "vigil-hold: [+{}ms] crtc {crtc} moved from fb {fb} to fb {now}: next owner has it",
                now_ms() - start
            );
        });
        match r {
            Ok(true) => return Released::HandedOver,
            Ok(false) => {}
            Err(e) => return Released::DeviceError(e),
        }
        if now_ms() - start >= timeout.as_millis() as u64 {
            return Released::TimedOut;
        }
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: i64::from(POLL_MS) * 1_000_000,
        };
        // SAFETY: nanosleep only reads the timespec.
        unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
    }
}

/// Close every descriptor except 0..=2 and the ones in `keep` (sorted).
fn close_others(keep: &[RawFd]) {
    let mut next: libc::c_uint = 3;
    for &k in keep.iter().filter(|k| **k >= 3) {
        let k = k as libc::c_uint;
        if k > next {
            close_range(next, k - 1);
        }
        next = next.max(k + 1);
    }
    close_range(next, libc::c_uint::MAX);
}

fn close_range(first: libc::c_uint, last: libc::c_uint) {
    // SAFETY: close_range(2) on descriptors this process no longer needs.
    let r = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
    if r < 0 {
        // Kernel older than 5.9: close one by one up to a sane bound.
        let top = last.min(4096);
        for fd in first..=top {
            // SAFETY: closing a possibly-unused descriptor is harmless.
            unsafe { libc::close(fd as RawFd) };
        }
    }
}

/// Drop master on every device and fork the holder. Returns its pid.
/// `Err` means nothing is held (and master may or may not be dropped, which
/// does not matter to an exiting process); the caller exits as usual.
pub fn start(devices: &[Device], timeout: Duration) -> Result<i32, String> {
    let held: Vec<Device> = devices
        .iter()
        .filter(|d| !d.crtcs.is_empty())
        .cloned()
        .collect();
    if held.is_empty() {
        return Err("no screen is scanning out a framebuffer".into());
    }
    if held.iter().map(|d| d.crtcs.len()).sum::<usize>() > 64 {
        return Err("more than 64 CRTCs".into());
    }
    // The fds must be usable before we promise anything.
    for d in &held {
        for &(crtc, fb) in &d.crtcs {
            match crtc_fb(d.fd, crtc) {
                Ok(now) if now == fb => {}
                Ok(now) => {
                    return Err(format!("crtc {crtc} shows fb {now}, not the expected {fb}"));
                }
                Err(e) => return Err(format!("GETCRTC on crtc {crtc}: errno {e}")),
            }
        }
    }
    for d in &held {
        match drop_master(d.fd) {
            Ok(true) => eprintln!("vigil: hold: dropped DRM master on fd {}", d.fd),
            Ok(false) => eprintln!(
                "vigil: hold: fd {} was not master (already released); nothing to drop",
                d.fd
            ),
            Err(e) => {
                return Err(format!(
                    "could not drop DRM master on fd {} (errno {e}); not holding, so the session cannot be blocked",
                    d.fd
                ));
            }
        }
    }
    let mut keep: Vec<RawFd> = held.iter().map(|d| d.fd).collect();
    keep.sort_unstable();

    // SAFETY: the child below calls only async-signal-safe libc functions
    // and writes to stack memory; it never returns into the parent's code.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(format!("fork failed: errno {}", errno())),
        0 => {
            // SAFETY: as above.
            unsafe {
                libc::setsid();
                libc::prctl(libc::PR_SET_NAME, c"vigil-hold".as_ptr(), 0, 0, 0);
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::dup2(null, 1);
                    if null > 2 {
                        libc::close(null);
                    }
                }
                libc::signal(libc::SIGPIPE, libc::SIG_IGN);
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
            close_others(&keep);
            let crtcs: usize = held.iter().map(|d| d.crtcs.len()).sum();
            say!(
                "vigil-hold: pid {} holding {crtcs} crtc(s) on {} device(s), timeout {} ms",
                unsafe { libc::getpid() },
                held.len(),
                timeout.as_millis()
            );
            let why = watch(&held, timeout);
            match why {
                Released::HandedOver => say!("vigil-hold: released, handed over"),
                Released::TimedOut => say!(
                    "vigil-hold: released, no next owner within {} ms; the screens go dark as they would have",
                    timeout.as_millis()
                ),
                Released::DeviceError(e) => say!("vigil-hold: released, device error {e}"),
            }
            // SAFETY: leaves without running the parent's atexit handlers.
            unsafe { libc::_exit(0) }
        }
        pid => Ok(pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(crtcs: &[(u32, u32)]) -> Device {
        Device {
            fd: 7,
            crtcs: crtcs.to_vec(),
        }
    }

    #[test]
    fn every_screen_must_change_before_the_hold_ends() {
        let devices = [dev(&[(1, 10), (2, 20)])];
        let mut left = [true, true];
        let mut seen = std::collections::HashMap::from([(1u32, 10u32), (2, 20)]);
        let mut moved = Vec::new();
        let mut read = |_: RawFd, c: u32| Ok(seen[&c]);
        assert_eq!(
            poll_once(&devices, &mut left, &mut read, |_, c, _, _| moved.push(c)),
            Ok(false)
        );
        seen.insert(1, 99);
        let mut read = |_: RawFd, c: u32| Ok(seen[&c]);
        // One screen taken over, the other still on vigil's frame: keep holding.
        assert_eq!(
            poll_once(&devices, &mut left, &mut read, |_, c, _, _| moved.push(c)),
            Ok(false)
        );
        // A screen the next owner turned off (fb 0) counts as handed over.
        seen.insert(2, 0);
        let mut read = |_: RawFd, c: u32| Ok(seen[&c]);
        assert_eq!(
            poll_once(&devices, &mut left, &mut read, |_, c, _, _| moved.push(c)),
            Ok(true)
        );
        assert_eq!(moved, vec![1, 2]);
    }

    #[test]
    fn a_device_error_ends_the_hold() {
        let devices = [dev(&[(1, 10)])];
        let mut left = [true];
        let mut read = |_: RawFd, _: u32| Err(libc::ENODEV);
        assert_eq!(
            poll_once(&devices, &mut left, &mut read, |_, _, _, _| {}),
            Err(libc::ENODEV)
        );
    }

    #[test]
    fn nothing_scanning_out_means_nothing_to_hold() {
        assert!(start(&[dev(&[])], Duration::from_secs(1)).is_err());
    }

    #[test]
    fn getcrtc_number_matches_the_kernel() {
        // _IOWR('d', 0xA1, 104-byte struct), as in drm.h.
        assert_eq!(std::mem::size_of::<ModeCrtc>(), 104);
        assert_eq!(GETCRTC, 0xC068_64A1);
    }
}
