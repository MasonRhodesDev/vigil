use std::os::fd::RawFd;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Device {
    pub fd: RawFd,
    pub crtcs: Vec<(u32, u32)>,
}

const POLL_MS: u32 = 20;

const DROP_MASTER: libc::c_ulong = 0x641f;
const GETCRTC: libc::c_ulong = (3 << 30) | (104 << 16) | (0x64 << 8) | 0xA1;

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
    // SAFETY: ModeCrtc is repr(C) with the 104-byte layout of drm_mode_crtc
    // that GETCRTC encodes, set_connectors_ptr is null, and `c` is a live local.
    let r = unsafe { libc::ioctl(fd, GETCRTC, &raw mut c) };
    if r < 0 { Err(errno()) } else { Ok(c.fb_id) }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq)]
enum Dropped {
    Yes,
    NotMaster,
    SeatManaged(i32),
}

fn drop_master(fd: RawFd) -> Result<Dropped, i32> {
    // SAFETY: DROP_MASTER takes no argument and only changes master on `fd`.
    let r = unsafe { libc::ioctl(fd, DROP_MASTER) };
    match (r, errno()) {
        (0, _) => Ok(Dropped::Yes),
        (_, libc::EINVAL) => Ok(Dropped::NotMaster),
        (_, e @ (libc::EACCES | libc::EPERM)) => Ok(Dropped::SeatManaged(e)),
        (_, e) => Err(e),
    }
}

fn now_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a live local timespec and CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

#[derive(Debug, PartialEq, Eq)]
pub enum Released {
    HandedOver,
    TimedOut,
    DeviceError(i32),
}

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
            vigil_log::say!(
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
        // SAFETY: ts is a live local and the remainder pointer may be null.
        unsafe { libc::nanosleep(&raw const ts, std::ptr::null_mut()) };
    }
}

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
    // SAFETY: only the forked hold child calls this, single threaded, and it
    // ends in _exit, so no Rust owner of a closed fd ever runs again.
    let r = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
    if r < 0 {
        let top = last.min(4096);
        for fd in first..=top {
            // SAFETY: as for close_range above; the fd may not be open, which close reports.
            unsafe { libc::close(fd as RawFd) };
        }
    }
}

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
            Ok(Dropped::Yes) => vigil_log::say!("vigil: hold: dropped DRM master on fd {}", d.fd),
            Ok(Dropped::NotMaster) => vigil_log::say!(
                "vigil: hold: fd {} was not master (already released); nothing to drop",
                d.fd
            ),
            Ok(Dropped::SeatManaged(e)) => vigil_log::say!(
                "vigil: hold: fd {} master is managed by the seat manager (errno {e}); holding the fd open, the seat manager drops master when it releases this session",
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

    // SAFETY: the child allocates nothing, calls only async-signal-safe
    // functions on data built before the fork, and leaves through _exit.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(format!("fork failed: errno {}", errno())),
        0 => {
            // SAFETY: setsid takes no arguments and moves only this child.
            unsafe { libc::setsid() };
            // SAFETY: PR_SET_NAME reads a NUL-terminated static string.
            unsafe { libc::prctl(libc::PR_SET_NAME, c"vigil-hold".as_ptr(), 0, 0, 0) };
            // SAFETY: the path is a NUL-terminated static string.
            let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
            if null >= 0 {
                // SAFETY: the single-threaded child replaces its stdin with /dev/null.
                unsafe { libc::dup2(null, 0) };
                // SAFETY: the single-threaded child replaces its stdout with /dev/null.
                unsafe { libc::dup2(null, 1) };
                if null > 2 {
                    // SAFETY: `null` is the fd opened above and nothing else holds it.
                    unsafe { libc::close(null) };
                }
            }
            // SAFETY: SIG_IGN installs no handler code.
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
            // SAFETY: SIG_IGN installs no handler code.
            unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            close_others(&keep);
            let crtcs: usize = held.iter().map(|d| d.crtcs.len()).sum();
            // SAFETY: getpid takes no arguments and cannot fail.
            let pid = unsafe { libc::getpid() };
            vigil_log::say!(
                "vigil-hold: pid {pid} holding {crtcs} crtc(s) on {} device(s), timeout {} ms",
                held.len(),
                timeout.as_millis()
            );
            let why = watch(&held, timeout);
            match why {
                Released::HandedOver => vigil_log::say!("vigil-hold: released, handed over"),
                Released::TimedOut => vigil_log::say!(
                    "vigil-hold: released, no next owner within {} ms; the screens go dark as they would have",
                    timeout.as_millis()
                ),
                Released::DeviceError(e) => {
                    vigil_log::say!("vigil-hold: released, device error {e}")
                }
            }
            // SAFETY: _exit ends the child without running the parent's
            // destructors, which would release resources the parent still owns.
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
        assert_eq!(
            poll_once(&devices, &mut left, &mut read, |_, c, _, _| moved.push(c)),
            Ok(false)
        );
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
        assert_eq!(std::mem::size_of::<ModeCrtc>(), 104);
        assert_eq!(GETCRTC, 0xC068_64A1);
    }
}
