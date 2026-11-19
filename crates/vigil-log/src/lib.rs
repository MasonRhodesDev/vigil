#![deny(unsafe_code)]

use std::fmt;
use std::io::Write as _;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

const MAX_LINE: usize = 2048;
const TRUNCATED: &[u8] = b" trunc=1";
const ROOM: usize = MAX_LINE - 1 - TRUNCATED.len();

/// Writes one line to stderr. It never panics, leaves errno as it was and
/// allocates nothing, though a `Display` impl that allocates still does. It
/// does not block, except in one race on a pipe that /proc cannot reopen,
/// described at `write_if_room`. Control characters are escaped as `\n`,
/// `\xNN` or, past ASCII, `\u{NN}`, so a message cannot start a second line.
/// A line longer than 2048 bytes is cut and ends in ` trunc=1`.
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
#[expect(
    clippy::disallowed_methods,
    reason = "out! is the one stdout writer, and it ignores every error"
)]
pub fn out(args: fmt::Arguments<'_>) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_fmt(args);
    let _ = stdout.write_all(b"\n");
}

/// Writes a finished record to stderr as is, on the same terms as `say!`.
pub fn emit(record: &[u8]) {
    let errno = errno();
    write_record(libc::STDERR_FILENO, record);
    sys::set_errno(errno);
}

fn line_to(fd: RawFd, args: fmt::Arguments<'_>) {
    let errno = errno();
    let mut line = Line {
        buf: [0; MAX_LINE],
        len: 0,
        cut: false,
    };
    let _ = fmt::Write::write_fmt(&mut line, args);
    write_record(fd, line.finish());
    sys::set_errno(errno);
}

struct Line {
    buf: [u8; MAX_LINE],
    len: usize,
    cut: bool,
}

impl Line {
    fn push_text(&mut self, text: &str) {
        let room = if self.cut { 0 } else { ROOM - self.len };
        let mut n = text.len().min(room);
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        self.append(&text.as_bytes()[..n]);
        self.cut |= n < text.len();
    }

    fn push_escape(&mut self, c: char) {
        let mut escape = [0u8; 8];
        let mut cursor = &mut escape[..];
        let _ = match c {
            '\n' => cursor.write_all(b"\\n"),
            c if c.is_ascii() => write!(cursor, "\\x{:02x}", u32::from(c)),
            c => write!(cursor, "\\u{{{:x}}}", u32::from(c)),
        };
        let left = cursor.len();
        let len = escape.len() - left;
        if self.cut || len > ROOM - self.len {
            self.cut = true;
        } else {
            self.append(&escape[..len]);
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn finish(&mut self) -> &[u8] {
        if self.cut {
            self.append(TRUNCATED);
        }
        self.append(b"\n");
        &self.buf[..self.len]
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, mut s: &str) -> fmt::Result {
        while let Some((at, c)) = s.char_indices().find(|(_, c)| c.is_control()) {
            self.push_text(&s[..at]);
            self.push_escape(c);
            s = &s[at + c.len_utf8()..];
        }
        self.push_text(s);
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
fn write_record(fd: RawFd, record: &[u8]) {
    let Some(kind) = sys::file_type(fd) else {
        return;
    };
    match kind {
        libc::S_IFSOCK => drain(record, |rest| {
            sys::send(fd, rest, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
        }),
        libc::S_IFIFO | libc::S_IFCHR => match reopen_nonblocking(fd) {
            Some(own) => drain(record, |rest| sys::write(own.as_raw_fd(), rest)),
            None if kind == libc::S_IFIFO => write_if_room(fd, record),
            None => {}
        },
        _ => drain(record, |rest| sys::write(fd, rest)),
    }
}

fn drain(mut rest: &[u8], mut write: impl FnMut(&[u8]) -> isize) {
    while !rest.is_empty() {
        match write(rest) {
            n if n > 0 => rest = &rest[n as usize..],
            n if n < 0 && errno() == libc::EINTR => {}
            _ => return,
        }
    }
}

// Best effort for a pipe /proc cannot reopen, such as one with no reader.
// A record within PIPE_BUF goes out whole or not at all, and poll said it
// fits, but another writer can fill the pipe between the poll and the
// write, and then this write blocks until a reader drains it. A tty has no
// such guarantee, so its line is dropped instead.
fn write_if_room(fd: RawFd, record: &[u8]) {
    if record.len() > libc::PIPE_BUF {
        return;
    }
    while writable_now(fd) {
        if sys::write(fd, record) >= 0 || errno() != libc::EINTR {
            return;
        }
    }
}

fn reopen_nonblocking(fd: RawFd) -> Option<OwnedFd> {
    let mut path = [0u8; 32];
    let mut cursor = &mut path[..];
    write!(cursor, "/proc/self/fd/{fd}\0").ok()?;
    let path = std::ffi::CStr::from_bytes_until_nul(&path).ok()?;
    sys::open(
        path,
        libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
    )
}

fn writable_now(fd: RawFd) -> bool {
    sys::poll_out(fd)
        .is_some_and(|revents| revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0)
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[allow(unsafe_code)]
mod sys {
    use std::ffi::CStr;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    pub fn file_type(fd: RawFd) -> Option<libc::mode_t> {
        // SAFETY: stat is plain C data, for which all zero bytes is a valid value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: st is a live, writable stat that fstat only fills.
        let ok = unsafe { libc::fstat(fd, &raw mut st) } == 0;
        ok.then_some(st.st_mode & libc::S_IFMT)
    }

    pub fn send(fd: RawFd, bytes: &[u8], flags: i32) -> isize {
        // SAFETY: send reads at most bytes.len() bytes from the live slice.
        unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), flags) }
    }

    pub fn write(fd: RawFd, bytes: &[u8]) -> isize {
        // SAFETY: write reads at most bytes.len() bytes from the live slice.
        unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) }
    }

    pub fn open(path: &CStr, flags: i32) -> Option<OwnedFd> {
        // SAFETY: path is NUL terminated and outlives the call.
        let raw = unsafe { libc::open(path.as_ptr(), flags) };
        // SAFETY: open just returned raw as a new fd that nothing else owns.
        (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw) })
    }

    pub fn poll_out(fd: RawFd) -> Option<i16> {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: pollfd is one live entry, and a zero timeout returns at once.
        let ready = unsafe { libc::poll(&raw mut pollfd, 1, 0) } == 1;
        ready.then_some(pollfd.revents)
    }

    pub fn set_errno(value: i32) {
        // SAFETY: __errno_location has no preconditions.
        let slot = unsafe { libc::__errno_location() };
        // SAFETY: the slot is this thread's errno, valid and aligned while the thread lives.
        unsafe { *slot = value };
    }

    #[cfg(test)]
    pub fn status_flags(fd: RawFd) -> i32 {
        // SAFETY: F_GETFL takes no argument and only reads the fd's flags.
        unsafe { libc::fcntl(fd, libc::F_GETFL) }
    }
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
            write_record(fd, record);
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
        sys::status_flags(fd)
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
        while sys::send(writer.as_raw_fd(), &chunk, libc::MSG_DONTWAIT) > 0 {}
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

    fn line_through_a_pipe(args: fmt::Arguments<'_>) -> String {
        let (mut reader, writer) = std::io::pipe().unwrap();
        line_to(writer.as_raw_fd(), args);
        drop(writer);
        let mut got = String::new();
        reader.read_to_string(&mut got).unwrap();
        got
    }

    #[test]
    fn a_long_line_is_cut_on_a_char_boundary_and_marked() {
        let long = "\u{e9}".repeat(MAX_LINE);
        let got = line_through_a_pipe(format_args!("x{long} tail"));
        assert!(got.len() <= MAX_LINE, "{} bytes", got.len());
        assert!(got.ends_with("\u{e9} trunc=1\n"), "{got:?}");
        assert!(!got.contains("tail"));
    }

    #[test]
    fn a_line_that_fills_the_room_exactly_is_not_marked() {
        let exact = "x".repeat(ROOM);
        assert_eq!(
            line_through_a_pipe(format_args!("{exact}")),
            format!("{exact}\n")
        );
    }

    #[test]
    fn a_newline_in_a_message_cannot_start_a_second_line() {
        assert_eq!(
            line_through_a_pipe(format_args!("pam: {}", "a\nb")),
            "pam: a\\nb\n"
        );
    }

    #[test]
    fn control_characters_are_escaped_so_they_cannot_drive_the_terminal() {
        assert_eq!(
            line_through_a_pipe(format_args!("{}", "\r\x1b[2J\x7f\u{85}\t\u{e9}")),
            "\\x0d\\x1b[2J\\x7f\\u{85}\\x09\u{e9}\n"
        );
    }

    #[test]
    fn an_escape_that_does_not_fit_is_dropped_whole() {
        let fill = "x".repeat(ROOM - 1);
        let got = line_through_a_pipe(format_args!("{fill}\n"));
        assert_eq!(got, format!("{fill} trunc=1\n"));
    }

    #[test]
    fn a_line_leaves_errno_as_it_found_it() {
        let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
        assert!(std::fs::File::open("/nonexistent/vigil-log").is_err());
        line_to(full.as_raw_fd(), format_args!("lost"));
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT)
        );
    }
}
