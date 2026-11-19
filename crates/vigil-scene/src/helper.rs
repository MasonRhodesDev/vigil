use std::fs::File;
use std::io::{Seek, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::io::Errno;
use rustix::process::{
    Pid, PidfdFlags, Resource, Rlimit, Signal, getrlimit, pidfd_open, pidfd_send_signal,
    set_parent_process_death_signal, setrlimit,
};

use crate::{Error, Format, MAX_EDGE, MAX_LOGO_BYTES, MAX_PIXELS, MAX_SPEC_BYTES, Mask, Request};

/// Where packages install the helper. A crate that calls it takes the path
/// from `installed_helper!`, so a build can move it.
pub const INSTALLED_HELPER: &str = "/usr/lib/vigil/vigil-scene";
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(1500);
pub const MAX_DEADLINE: Duration = Duration::from_secs(5);

const MEMORY: u64 = 1 << 30;
const MESSAGE: u64 = 4096;

/// The helper's installed path, fixed when the calling crate is built: the
/// `VIGIL_SCENE_HELPER` build variable, else `INSTALLED_HELPER`. It is never
/// looked up on `PATH`.
#[macro_export]
macro_rules! installed_helper {
    () => {
        match option_env!("VIGIL_SCENE_HELPER") {
            Some(path) => path,
            None => $crate::INSTALLED_HELPER,
        }
    };
}

/// Draws untrusted logos in a child process: the `vigil-scene` binary, run
/// with a clean environment, limits it sets on itself before it reads a
/// byte, and a deadline after which it is killed and reaped. Whatever the
/// logo does, a call returns within the deadline, plus the kernel's teardown
/// of a killed helper and the copy of the result, and the caller's memory
/// grows by the result alone.
#[derive(Debug, Clone)]
pub struct Helper {
    path: PathBuf,
    deadline: Duration,
}

impl Helper {
    pub fn new(path: impl Into<PathBuf>, deadline: Duration) -> Helper {
        Helper {
            path: path.into(),
            deadline,
        }
    }

    pub fn mask(&self, logo: &[u8], format: Format, edge: u32) -> Result<Mask, Error> {
        if edge == 0 || edge > MAX_EDGE {
            return Err(Error::TooLarge(format!(
                "a logo {edge} px wide is outside 1..={MAX_EDGE}"
            )));
        }
        if logo.len() as u64 > format.cap() {
            return Err(Error::TooLarge(format!(
                "the logo is over {} bytes",
                format.cap()
            )));
        }
        let mut alpha = vec![0u8; edge as usize * edge as usize];
        self.run(
            &[
                "mask",
                "--format",
                format.name(),
                "--edge",
                &edge.to_string(),
            ],
            &[logo],
            &mut alpha,
        )?;
        Ok(Mask::from_alpha(edge, alpha))
    }

    /// Fills `out`, which must be exactly the layer's size, and writes
    /// nothing to it unless the drawing succeeded. A request without a logo
    /// has nothing untrusted to draw and is drawn here.
    pub fn draw(&self, req: &Request<'_>, out: &mut [u8]) -> Result<(), Error> {
        let need = req.bytes()?;
        if out.len() != need {
            return Err(Error::Invalid(format!(
                "the {} needs {need} bytes, not {}",
                req.layer.name(),
                out.len()
            )));
        }
        let Some(svg) = req.svg else {
            out.copy_from_slice(&req.draw()?);
            return Ok(());
        };
        let spec = req.spec.unwrap_or_default();
        if svg.len() as u64 > MAX_LOGO_BYTES || spec.len() as u64 > MAX_SPEC_BYTES {
            return Err(Error::TooLarge(
                "the logo or the scene spec is too big".into(),
            ));
        }
        let (mode, transform) = (format!("{}x{}", req.mode.0, req.mode.1), req.transform);
        self.run(
            &[
                "layer",
                "--mode",
                &mode,
                "--transform",
                &transform.to_string(),
                "--layer",
                req.layer.name(),
                "--value",
                &req.value.to_string(),
                "--svg-len",
                &svg.len().to_string(),
                "--spec-len",
                &spec.len().to_string(),
            ],
            &[svg, spec],
            out,
        )
    }

    fn run(&self, args: &[&str], input: &[&[u8]], out: &mut [u8]) -> Result<(), Error> {
        let by = Instant::now() + self.deadline;
        let mut stdin = memfd("vigil-scene-in")?;
        for part in input {
            stdin.write_all(part).map_err(io)?;
        }
        stdin.rewind().map_err(io)?;
        let (stdout, stderr) = (memfd("vigil-scene-out")?, memfd("vigil-scene-err")?);
        let mut child = Command::new(&self.path)
            .args(args)
            .args(["--deadline-ms", &self.deadline.as_millis().to_string()])
            .env_clear()
            .current_dir("/")
            .stdin(stdin)
            .stdout(stdout.try_clone().map_err(io)?)
            .stderr(stderr.try_clone().map_err(io)?)
            .spawn()
            .map_err(|e| Error::Helper(format!("cannot run {}: {e}", self.path.display())))?;
        // A host that ignores SIGCHLD has the kernel reap the helper as it
        // exits, which loses its status and can beat pidfd_open.
        let watch = |e: Errno| Error::Helper(format!("cannot watch the helper: {e}"));
        let exited = match pidfd_open(Pid::from_child(&child), PidfdFlags::empty()) {
            Err(Errno::SRCH) => Ok(true),
            Err(e) => Err(watch(e)),
            Ok(pidfd) => {
                let exited = wait(&pidfd, by);
                if !matches!(exited, Ok(true)) {
                    let _ = pidfd_send_signal(&pidfd, Signal::KILL);
                }
                exited
            }
        };
        if exited.is_err() {
            let _ = child.kill();
        }
        let status = match child.wait() {
            Ok(status) => Some(status),
            Err(e) if e.raw_os_error() == Some(Errno::CHILD.raw_os_error()) => None,
            Err(e) => return Err(io(e)),
        };
        if !exited? {
            return Err(Error::Refused(format!(
                "the helper was stopped at its {} ms deadline",
                self.deadline.as_millis()
            )));
        }
        match status.map(|s| (s.code(), s.signal())) {
            Some((Some(0), _)) => {}
            Some((Some(code), _)) => return Err(Error::from_exit(code, message(&stderr))),
            Some((None, signal)) => {
                return Err(Error::Refused(format!(
                    "the helper was killed by signal {}",
                    signal.unwrap_or(0)
                )));
            }
            None => {
                let why = message(&stderr);
                if !why.is_empty() {
                    return Err(Error::Refused(why));
                }
            }
        }
        let len = stdout.metadata().map_err(io)?.len();
        if len != out.len() as u64 {
            return Err(Error::Invalid(format!(
                "the helper wrote {len} bytes, not {}",
                out.len()
            )));
        }
        stdout.read_exact_at(out, 0).map_err(io)
    }
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

fn memfd(name: &str) -> Result<File, Error> {
    memfd_create(name, MemfdFlags::CLOEXEC)
        .map(File::from)
        .map_err(|e| Error::Io(format!("memfd: {e}")))
}

fn wait(pidfd: &OwnedFd, by: Instant) -> Result<bool, Error> {
    loop {
        let left = by.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let left = Timespec::try_from(left)
            .map_err(|_| Error::Helper("the deadline is out of range".into()))?;
        match poll(&mut [PollFd::new(pidfd, PollFlags::IN)], Some(&left)) {
            Ok(0) | Err(Errno::INTR) => {}
            Ok(_) => return Ok(true),
            Err(e) => return Err(Error::Helper(format!("cannot watch the helper: {e}"))),
        }
    }
}

fn message(stderr: &File) -> String {
    let len = stderr.metadata().map_or(0, |m| m.len()).min(MESSAGE);
    let mut buf = vec![0u8; len as usize];
    let read = stderr.read_at(&mut buf, 0).unwrap_or(0);
    String::from_utf8_lossy(&buf[..read])
        .trim()
        .trim_start_matches("vigil-scene: ")
        .to_owned()
}

/// What the helper does to itself before it reads its input: CPU time just
/// past the deadline, 1 GiB of address space, no descriptors beyond stdio,
/// output no bigger than the largest frame, no core dumps and no children.
/// It dies with the thread that started it and can never gain privileges.
/// A limit already lower is kept.
pub fn confine(deadline: Duration) -> Result<(), Error> {
    let cpu = u64::try_from(deadline.as_millis().div_ceil(1000))
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    for (resource, want) in [
        (Resource::Cpu, cpu),
        (Resource::As, MEMORY),
        (Resource::Nofile, 3),
        (Resource::Fsize, MAX_PIXELS * 4 + MESSAGE),
        (Resource::Core, 0),
        (Resource::Nproc, 0),
    ] {
        let limit = getrlimit(resource).maximum.map_or(want, |m| m.min(want));
        setrlimit(
            resource,
            Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )
        .map_err(|e| Error::Helper(format!("limiting {resource:?}: {e}")))?;
    }
    set_parent_process_death_signal(Some(Signal::KILL))
        .and_then(|()| rustix::thread::set_no_new_privs(true))
        .map_err(|e| Error::Helper(format!("confining the helper: {e}")))
}
