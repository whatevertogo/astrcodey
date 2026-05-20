//! Short, best-effort terminal response probes for TUI startup.
//!
//! Crossterm's public helpers wait up to two seconds for terminal responses. That is too long for
//! TUI startup, where unsupported terminals should simply fall back to conservative defaults.
//! This module sends the same kinds of optional terminal queries with a caller-provided deadline,
//! prefers duplicated stdio handles, falls back to the controlling terminal path when stdio is
//! unavailable, and reports `None` when a response is unavailable.
//!
//! TODO: 非阻塞读取在探测期间可能消费掉属于后续正常输入的数据。
//! 目前时序上无影响（探测发生在 TUI 事件循环启动之前），但若探测时机改动
//! 则可能丢失按键。未来可考虑将消费到的多余字节推回事件队列。

#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
mod imp {
    use std::{
        fs::{File, OpenOptions},
        io,
        io::Write,
        os::fd::{AsRawFd, FromRawFd},
        time::{Duration, Instant},
    };

    use ratatui::layout::Position;

    /// Default timeout for terminal response probes during startup.
    ///
    /// Kept intentionally short — unsupported terminals should fall back quickly
    /// rather than blocking the TUI initialization.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);

    /// Temporary terminal handle used while a startup probe owns terminal input.
    ///
    /// The preferred path is duplicated stdin/stdout, because terminal replies are delivered to the
    /// same input stream crossterm reads from. Some embedded or redirected environments expose a
    /// controlling terminal without terminal stdio; in that case the handle falls back to
    /// `/dev/tty`. Only the reader is switched to nonblocking mode, and its original file status
    /// flags are restored when the handle is dropped.
    struct Tty {
        reader: File,
        writer: File,
        original_flags: libc::c_int,
    }

    impl Tty {
        /// Opens an isolated reader and writer for startup probes.
        ///
        /// The reader and writer must be separate file descriptions so switching the reader into
        /// nonblocking mode does not also make writes fail with `WouldBlock` under terminal
        /// backpressure. Falling back to `/dev/tty` keeps embedded or redirected environments
        /// usable when they still expose a controlling terminal.
        fn open() -> io::Result<Self> {
            let stdio_reader = dup_file(libc::STDIN_FILENO);
            let stdio_writer = dup_file(libc::STDOUT_FILENO);
            match (stdio_reader, stdio_writer) {
                (Ok(reader), Ok(writer)) => Self::new(reader, writer),
                (reader, writer) => {
                    let stdio_err = match (reader.err(), writer.err()) {
                        (Some(reader_err), Some(writer_err)) => {
                            format!("reader: {reader_err}; writer: {writer_err}")
                        },
                        (Some(reader_err), None) => format!("reader: {reader_err}"),
                        (None, Some(writer_err)) => format!("writer: {writer_err}"),
                        (None, None) => "unknown stdio duplicate error".to_string(),
                    };
                    let reader =
                        OpenOptions::new()
                            .read(true)
                            .open("/dev/tty")
                            .map_err(|fallback_err| {
                                io::Error::new(
                                    fallback_err.kind(),
                                    format!(
                                        "failed to duplicate stdio ({stdio_err}) or open /dev/tty \
                                         reader ({fallback_err})"
                                    ),
                                )
                            })?;
                    let writer = OpenOptions::new().write(true).open("/dev/tty").map_err(
                        |fallback_err| {
                            io::Error::new(
                                fallback_err.kind(),
                                format!(
                                    "failed to duplicate stdio ({stdio_err}) or open /dev/tty \
                                     writer ({fallback_err})"
                                ),
                            )
                        },
                    )?;
                    Self::new(reader, writer)
                },
            }
        }

        fn new(reader: File, writer: File) -> io::Result<Self> {
            let fd = reader.as_raw_fd();
            let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if original_flags == -1 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                reader,
                writer,
                original_flags,
            })
        }

        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writer.write_all(bytes)?;
            self.writer.flush()
        }

        fn read_available(&mut self, buffer: &mut Vec<u8>) -> io::Result<()> {
            let mut chunk = [0_u8; 256];
            loop {
                let count = unsafe {
                    libc::read(
                        self.reader.as_raw_fd(),
                        chunk.as_mut_ptr().cast::<libc::c_void>(),
                        chunk.len(),
                    )
                };
                if count > 0 {
                    buffer.extend_from_slice(&chunk[..count as usize]);
                    continue;
                }
                if count == 0 {
                    return Ok(());
                }
                let err = io::Error::last_os_error();
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Ok(());
                }
                return Err(err);
            }
        }

        fn poll_readable(&self, timeout: Duration) -> io::Result<bool> {
            let mut fd = libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let deadline = Instant::now() + timeout;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(false);
                }
                let timeout_ms = deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .min(i32::MAX as u128) as i32;
                let result = unsafe {
                    libc::poll(&mut fd, /* nfds */ 1, timeout_ms)
                };
                if result > 0 {
                    return Ok((fd.revents & libc::POLLIN) != 0);
                }
                if result == 0 {
                    return Ok(false);
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            let _ =
                unsafe { libc::fcntl(self.reader.as_raw_fd(), libc::F_SETFL, self.original_flags) };
        }
    }

    /// Duplicates a process stdio descriptor so probe cleanup owns only the duplicate.
    fn dup_file(fd: libc::c_int) -> io::Result<File> {
        let duplicated = unsafe { libc::dup(fd) };
        if duplicated == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(duplicated) })
    }

    /// Queries the current cursor position and returns a zero-based Ratatui position.
    ///
    /// A timeout or a non-CPR response is not fatal. Callers should treat `Ok(None)` as "terminal
    /// did not answer this optional query" and choose a conservative fallback.
    pub fn cursor_position(timeout: Duration) -> io::Result<Option<Position>> {
        let mut tty = Tty::open()?;
        tty.write_all(b"\x1B[6n")?;
        let Some(response) = read_until(&mut tty, timeout, parse_cursor_position)? else {
            return Ok(None);
        };
        Ok(Some(response))
    }

    /// Reads available terminal bytes until `parse` recognizes a probe response or time expires.
    ///
    /// The accumulated buffer may include unrelated terminal input. This helper intentionally does
    /// not try to replay those bytes, so it must stay limited to short startup probes that run
    /// before normal crossterm input polling begins.
    fn read_until<T>(
        tty: &mut Tty,
        timeout: Duration,
        mut parse: impl FnMut(&[u8]) -> Option<T>,
    ) -> io::Result<Option<T>> {
        let deadline = Instant::now() + timeout;
        let mut buffer = Vec::new();
        loop {
            tty.read_available(&mut buffer)?;
            if let Some(value) = parse(&buffer) {
                return Ok(Some(value));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if !tty.poll_readable(deadline.saturating_duration_since(now))? {
                return Ok(None);
            }
        }
    }

    fn parse_cursor_position(buffer: &[u8]) -> Option<Position> {
        for start in find_all_subslices(buffer, b"\x1B[") {
            let rest = &buffer[start + 2..];
            let Some(end) = rest.iter().position(|b| *b == b'R') else {
                continue;
            };
            let Ok(payload) = std::str::from_utf8(&rest[..end]) else {
                continue;
            };
            let Some((row, col)) = payload.split_once(';') else {
                continue;
            };
            let Ok(row) = row.parse::<u16>() else {
                continue;
            };
            let Ok(col) = col.parse::<u16>() else {
                continue;
            };
            let row = row.saturating_sub(1);
            let col = col.saturating_sub(1);
            return Some(Position { x: col, y: row });
        }
        None
    }

    fn find_all_subslices<'a>(
        haystack: &'a [u8],
        needle: &'a [u8],
    ) -> impl Iterator<Item = usize> + 'a {
        haystack
            .windows(needle.len())
            .enumerate()
            .filter_map(move |(idx, window)| (window == needle).then_some(idx))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_cursor_position_as_zero_based() {
            assert_eq!(
                parse_cursor_position(b"\x1B[20;10R"),
                Some(Position { x: 9, y: 19 })
            );
            assert_eq!(
                parse_cursor_position(b"\x1B[I\x1B[20;10R"),
                Some(Position { x: 9, y: 19 })
            );
        }
    }
}

#[cfg(unix)]
pub use imp::*;
