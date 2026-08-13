//! Color-scheme reports: CSI 996 query, CSI 997 reply, DECSET 2031.
//!
//! Crossterm 0.29 treats an unknown `CSI ? …` final byte as "need more
//! data" (crossterm#1104). An unsolicited `CSI ? 997 ; 1 n` would stall
//! `EventStream` and swallow later keys. This module strips those
//! reports from the tty and yields Dark/Light on a channel instead.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::openpty;
use nix::unistd::{dup, dup2, isatty, pipe};
use nix::unistd::{read, write};

/// Terminal color preference from a CSI 997 DSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    Dark,
    Light,
}

const ENABLE: &[u8] = b"\x1b[?2031h";
const DISABLE: &[u8] = b"\x1b[?2031l";
const QUERY: &[u8] = b"\x1b[?996n";

pub(crate) fn set_reports_enabled(enabled: bool) {
    let mut out = io::stdout();
    let _ = out.write_all(if enabled { ENABLE } else { DISABLE });
    let _ = out.flush();
}

/// Write `CSI ? 996 n`. The 997 reply is read by `query_color_scheme`
/// before the interceptor, or by the watch after it.
pub(crate) fn request_color_scheme() {
    let mut out = io::stdout();
    let _ = out.write_all(QUERY);
    let _ = out.flush();
}

/// Ask the terminal for the current scheme. Same stdin contract as OSC 11:
/// raw mode on, `EventStream` dropped. Timeout or a missed reply is `None`.
pub(crate) fn query_color_scheme(timeout: Duration) -> Option<ColorScheme> {
    request_color_scheme();

    let stdin = io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    #[allow(unsafe_code)]
    let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(stdin_fd) };
    let deadline = Instant::now() + timeout;
    let mut pending = Vec::new();
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let Ok(timeout_ms) = PollTimeout::try_from(remaining) else {
            return None;
        };
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, timeout_ms) {
            Ok(n) if n > 0 => {}
            _ => return None,
        }
        let mut chunk = [0u8; 64];
        match read(stdin_fd, &mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(m) => {
                let (_, reports) = take_color_scheme_reports(&mut pending, &chunk[..m]);
                if let Some(scheme) = reports.into_iter().next() {
                    return Some(scheme);
                }
                if pending.len() > 64 {
                    return None;
                }
            }
        }
    }
}

enum Classify {
    NeedMore,
    Report(ColorScheme, usize),
    /// Complete unused 997 variant. Drop so crossterm does not stall.
    Drop(usize),
    /// Not a 997 report. `n` is how many leading bytes to emit unchanged.
    Forward(usize),
}

fn classify_csi_997(buf: &[u8]) -> Classify {
    if buf.is_empty() {
        return Classify::NeedMore;
    }
    if buf[0] != 0x1b {
        return Classify::Forward(1);
    }
    if buf.len() < 2 {
        return Classify::NeedMore;
    }
    if buf[1] != b'[' {
        return Classify::Forward(1);
    }
    if buf.len() < 3 {
        return Classify::NeedMore;
    }
    if buf[2] != b'?' {
        return Classify::Forward(1);
    }
    let mut end = 3;
    while end < buf.len() && !(0x40..=0x7e).contains(&buf[end]) {
        end += 1;
    }
    if end >= buf.len() {
        return Classify::NeedMore;
    }
    let params = &buf[3..end];
    let len = end + 1;
    if buf[end] == b'n' && params.starts_with(b"997;") {
        return match params {
            b"997;1" => Classify::Report(ColorScheme::Dark, len),
            b"997;2" => Classify::Report(ColorScheme::Light, len),
            _ => Classify::Drop(len),
        };
    }
    Classify::Forward(len)
}

/// Split complete `CSI ? 997 ; N n` reports out of the byte stream.
/// Incomplete CSI at the tail stays in `pending`.
pub(crate) fn take_color_scheme_reports(
    pending: &mut Vec<u8>,
    input: &[u8],
) -> (Vec<u8>, Vec<ColorScheme>) {
    pending.extend_from_slice(input);
    let mut out = Vec::with_capacity(pending.len());
    let mut reports = Vec::new();
    let mut i = 0;
    while i < pending.len() {
        match classify_csi_997(&pending[i..]) {
            Classify::NeedMore => break,
            Classify::Report(scheme, n) => {
                reports.push(scheme);
                i += n;
            }
            Classify::Drop(n) => i += n,
            Classify::Forward(n) => {
                out.extend_from_slice(&pending[i..i + n]);
                i += n;
            }
        }
    }
    pending.drain(..i);
    (out, reports)
}

/// Intercepts tty input so 997 reports never reach `EventStream`.
pub(crate) struct ColorSchemeWatch {
    rx: tokio::sync::mpsc::UnboundedReceiver<ColorScheme>,
    restore: Option<WatchRestore>,
}

struct WatchRestore {
    orig_stdin: OwnedFd,
    stop: Arc<AtomicBool>,
    stop_w: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl ColorSchemeWatch {
    pub(crate) fn install() -> Self {
        match install_filter() {
            Ok(watch) => watch,
            Err(_) => Self::inactive(),
        }
    }

    fn inactive() -> Self {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { rx, restore: None }
    }

    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        self.restore.is_some()
    }

    pub(crate) async fn recv(&mut self) -> Option<ColorScheme> {
        if self.restore.is_none() {
            std::future::pending::<()>().await;
            return None;
        }
        self.rx.recv().await
    }
}

impl Drop for ColorSchemeWatch {
    fn drop(&mut self) {
        let Some(mut restore) = self.restore.take() else {
            return;
        };
        set_reports_enabled(false);
        restore.stop.store(true, Ordering::Relaxed);
        let _ = write(&restore.stop_w, &[1u8]);
        if let Some(thread) = restore.thread.take() {
            let _ = thread.join();
        }
        let _ = dup2(restore.orig_stdin.as_raw_fd(), 0);
    }
}

fn install_filter() -> io::Result<ColorSchemeWatch> {
    if !isatty(0).unwrap_or(false) {
        return Err(io::Error::other("stdin is not a tty"));
    }
    let orig_raw = dup(0).map_err(io_err)?;
    #[allow(unsafe_code)]
    // SAFETY: `dup` returned a fresh fd we now own.
    let orig_stdin = unsafe { OwnedFd::from_raw_fd(orig_raw) };
    let thread_in_raw = dup(orig_stdin.as_raw_fd()).map_err(io_err)?;
    #[allow(unsafe_code)]
    let thread_in = unsafe { OwnedFd::from_raw_fd(thread_in_raw) };

    let pty = openpty(None, None).map_err(io_err)?;
    dup2(pty.slave.as_raw_fd(), 0).map_err(io_err)?;
    drop(pty.slave);

    let (stop_r, stop_w) = pipe().map_err(io_err)?;
    set_nonblock(stop_r.as_raw_fd())?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let master = pty.master;
    let thread = std::thread::Builder::new()
        .name("lofi-color-scheme".into())
        .spawn(move || filter_loop(thread_in, master, stop_r, stop_flag, tx))
        .map_err(io::Error::other)?;

    Ok(ColorSchemeWatch {
        rx,
        restore: Some(WatchRestore {
            orig_stdin,
            stop,
            stop_w,
            thread: Some(thread),
        }),
    })
}

fn set_nonblock(fd: RawFd) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io_err)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io_err)?;
    Ok(())
}

fn io_err(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

fn filter_loop(
    input: OwnedFd,
    master: OwnedFd,
    stop_r: OwnedFd,
    stop: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<ColorScheme>,
) {
    let mut pending = Vec::new();
    let mut buf = [0u8; 256];
    while !stop.load(Ordering::Relaxed) {
        use std::os::fd::AsFd;
        let mut fds = [
            PollFd::new(input.as_fd(), PollFlags::POLLIN),
            PollFd::new(stop_r.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Err(_) => break,
            Ok(_) => {}
        }
        if stop.load(Ordering::Relaxed)
            || fds[1]
                .revents()
                .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP))
        {
            break;
        }
        match read(input.as_raw_fd(), &mut buf) {
            Ok(0) | Err(Errno::EBADF) => break,
            Err(Errno::EINTR | Errno::EAGAIN) => continue,
            Err(_) => break,
            Ok(n) => {
                let (forward, reports) = take_color_scheme_reports(&mut pending, &buf[..n]);
                for scheme in reports {
                    if tx.send(scheme).is_err() {
                        return;
                    }
                }
                if !forward.is_empty() && write_all(&master, &forward).is_err() {
                    break;
                }
            }
        }
    }
}

fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        match write(fd, bytes) {
            Ok(0) => return Err(Errno::EIO),
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_dark_and_forwards_keys() {
        let mut pending = Vec::new();
        let (out, reports) = take_color_scheme_reports(&mut pending, b"a\x1b[?997;1nb");
        assert_eq!(out, b"ab");
        assert_eq!(reports, vec![ColorScheme::Dark]);
        assert!(pending.is_empty());
    }

    #[test]
    fn extracts_light() {
        let mut pending = Vec::new();
        let (out, reports) = take_color_scheme_reports(&mut pending, b"\x1b[?997;2n");
        assert!(out.is_empty());
        assert_eq!(reports, vec![ColorScheme::Light]);
    }

    #[test]
    fn splits_across_chunks() {
        let mut pending = Vec::new();
        let (out, reports) = take_color_scheme_reports(&mut pending, b"\x1b[?99");
        assert!(out.is_empty());
        assert!(reports.is_empty());
        let (out, reports) = take_color_scheme_reports(&mut pending, b"7;1nX");
        assert_eq!(out, b"X");
        assert_eq!(reports, vec![ColorScheme::Dark]);
        assert!(pending.is_empty());
    }

    #[test]
    fn forwards_other_csi() {
        let mut pending = Vec::new();
        let seq = b"\x1b[I";
        let (out, reports) = take_color_scheme_reports(&mut pending, seq);
        assert_eq!(out, seq);
        assert!(reports.is_empty());
    }

    #[test]
    fn drops_unknown_997_variant() {
        let mut pending = Vec::new();
        let (out, reports) = take_color_scheme_reports(&mut pending, b"x\x1b[?997;3ny");
        assert_eq!(out, b"xy");
        assert!(reports.is_empty());
    }
}
