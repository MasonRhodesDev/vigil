use std::fmt;
use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const MAX_LINE: usize = 2048;

#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::line(::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out(::core::format_args!($($arg)*))
    };
}

#[doc(hidden)]
pub fn line(args: fmt::Arguments<'_>) {
    line_to(libc::STDERR_FILENO, args);
}

#[doc(hidden)]
pub fn out(args: fmt::Arguments<'_>) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_fmt(args);
    let _ = stdout.write_all(b"\n");
}

pub fn emit(record: &[u8]) {
    emit_to(libc::STDERR_FILENO, record);
}

fn line_to(fd: RawFd, args: fmt::Arguments<'_>) {
    let mut line = Line {
        buf: [0; MAX_LINE],
        len: 0,
        full: false,
    };
    let _ = fmt::Write::write_fmt(&mut line, args);
    line.buf[line.len] = b'\n';
    emit_to(fd, &line.buf[..=line.len]);
}

struct Line {
    buf: [u8; MAX_LINE],
    len: usize,
    full: bool,
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.full {
            return Ok(());
        }
        let room = MAX_LINE - 1 - self.len;
        let mut n = s.len().min(room);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        self.full = n < s.len();
        Ok(())
    }
}

// O_NONBLOCK lives on the open file description, which a dup shares with
// fd 2, the parent shell's terminal and every C writer in the process, so
// it is never set on fd 2. Sockets get MSG_DONTWAIT per call; pipes and
// ttys get a private description opened through /proc; regular files
// cannot stall and must keep fd 2's offset, so they are written directly.
// Nothing is cached, because a forked child may close or reuse any fd but
// 0 to 2. No allocation either, for the same forked child.
fn emit_to(fd: RawFd, record: &[u8]) {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return;
    }
    match st.st_mode & libc::S_IFMT {
        libc::S_IFSOCK => drain(record, |rest| unsafe {
            libc::send(
                fd,
                rest.as_ptr().cast(),
                rest.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        }),
        libc::S_IFIFO | libc::S_IFCHR => match reopen_nonblocking(fd) {
            Some(own) => drain(record, |rest| unsafe {
                libc::write(own.as_raw_fd(), rest.as_ptr().cast(), rest.len())
            }),
            None if writable_now(fd) => {
                unsafe { libc::write(fd, record.as_ptr().cast(), record.len()) };
            }
            None => {}
        },
        _ => drain(record, |rest| unsafe {
            libc::write(fd, rest.as_ptr().cast(), rest.len())
        }),
    }
}

fn drain(mut rest: &[u8], mut write: impl FnMut(&[u8]) -> isize) {
    while !rest.is_empty() {
        match write(rest) {
            n if n > 0 => rest = &rest[n as usize..],
            n if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => {}
            _ => return,
        }
    }
}

fn reopen_nonblocking(fd: RawFd) -> Option<OwnedFd> {
    let mut path = [0u8; 32];
    let mut cursor = &mut path[..];
    write!(cursor, "/proc/self/fd/{fd}\0").ok()?;
    let raw = unsafe {
        libc::open(
            path.as_ptr().cast(),
            libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw) })
}

fn writable_now(fd: RawFd) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pollfd, 1, 0) } == 1;
    ready && pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    fn returns_promptly(fd: RawFd, record: &'static [u8]) -> bool {
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            emit_to(fd, record);
            let _ = done.send(());
        });
        finished.recv_timeout(Duration::from_secs(5)).is_ok()
    }

    fn fill_pipe(fd: RawFd) {
        let mut own = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(format!("/proc/self/fd/{fd}"))
            .unwrap();
        for chunk in [&[b'x'; 4096][..], b"x"] {
            while own.write(chunk).is_ok() {}
        }
    }

    fn flags(fd: RawFd) -> i32 {
        unsafe { libc::fcntl(fd, libc::F_GETFL) }
    }

    #[test]
    fn a_pipe_with_no_reader_drops_the_line() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        assert!(returns_promptly(writer.as_raw_fd(), b"lost\n"));
    }

    #[test]
    fn a_full_disk_drops_the_line() {
        let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
        assert!(returns_promptly(full.as_raw_fd(), b"lost\n"));
    }

    #[test]
    fn a_closed_fd_drops_the_line() {
        assert!(returns_promptly(-1, b"lost\n"));
    }

    #[test]
    fn a_pipe_nobody_drains_drops_the_line_instead_of_blocking() {
        let (_reader, writer) = std::io::pipe().unwrap();
        fill_pipe(writer.as_raw_fd());
        assert_eq!(flags(writer.as_raw_fd()) & libc::O_NONBLOCK, 0);
        assert!(returns_promptly(writer.as_raw_fd(), b"lost\n"));
    }

    #[test]
    fn a_socket_nobody_drains_drops_the_line_instead_of_blocking() {
        let (_reader, writer) = UnixStream::pair().unwrap();
        let chunk = [b'x'; 4096];
        while unsafe {
            libc::send(
                writer.as_raw_fd(),
                chunk.as_ptr().cast(),
                chunk.len(),
                libc::MSG_DONTWAIT,
            )
        } > 0
        {}
        assert_eq!(flags(writer.as_raw_fd()) & libc::O_NONBLOCK, 0);
        assert!(returns_promptly(writer.as_raw_fd(), b"lost\n"));
    }

    #[test]
    fn a_line_reaches_a_pipe_whole_and_leaves_its_flags_alone() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let before = flags(writer.as_raw_fd());
        line_to(
            writer.as_raw_fd(),
            format_args!("vigil-lock: {} {}", "session", 7),
        );
        assert_eq!(flags(writer.as_raw_fd()), before);
        drop(writer);
        let mut got = String::new();
        reader.read_to_string(&mut got).unwrap();
        assert_eq!(got, "vigil-lock: session 7\n");
    }

    #[test]
    fn a_line_reaches_a_socket_whole() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        line_to(writer.as_raw_fd(), format_args!("journal {}", 1));
        drop(writer);
        let mut got = String::new();
        reader.read_to_string(&mut got).unwrap();
        assert_eq!(got, "journal 1\n");
    }

    #[test]
    fn a_regular_file_is_appended_at_its_offset_not_overwritten() {
        let path = std::env::temp_dir().join(format!("vigil-log-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(b"earlier\n").unwrap();
        line_to(file.as_raw_fd(), format_args!("later"));
        let got = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, "earlier\nlater\n");
    }

    #[test]
    fn a_long_line_is_cut_on_a_char_boundary_and_still_ends_the_line() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let long = "\u{e9}".repeat(MAX_LINE);
        line_to(writer.as_raw_fd(), format_args!("x{long} tail"));
        drop(writer);
        let mut got = String::new();
        reader.read_to_string(&mut got).unwrap();
        assert!(got.len() <= MAX_LINE, "{} bytes", got.len());
        assert!(got.ends_with("\u{e9}\n"));
        assert!(!got.contains("tail"));
    }
}
