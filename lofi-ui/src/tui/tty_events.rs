//! TTY input events, including terminal color reports.
//!
//! Crossterm 0.29 treats a finished unknown `CSI ? …` as incomplete
//! (crossterm#1104) and then swallows later keys. We read the tty and
//! parse here so 997 is a real event and other private reports are
//! dropped. jjui does the same in bubbletea/ultraviolet.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossterm::event::Event;
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{pipe, read, write};

mod parse;
use parse::Parsed;

const ENABLE_2031: &[u8] = b"\x1b[?2031h";
const DISABLE_2031: &[u8] = b"\x1b[?2031l";

pub(crate) fn set_reports_enabled(enabled: bool) {
    let mut out = io::stdout();
    let _ = out.write_all(if enabled { ENABLE_2031 } else { DISABLE_2031 });
    let _ = out.flush();
}

pub(crate) fn request_background() {
    let _ = crate::tui::terminal_bg::request_background();
}

#[derive(Debug)]
pub(crate) enum TuiEvent {
    Input(Event),
    ColorSchemeChanged,
    Background(crate::tui::terminal_bg::Rgb),
}

/// Blocking tty reader that yields [`TuiEvent`]s on a channel.
pub(crate) struct TtyEvents {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<TuiEvent, io::Error>>,
    pending: VecDeque<Result<TuiEvent, io::Error>>,
    restore: Option<ReaderRestore>,
}

struct ReaderRestore {
    stop: Arc<AtomicBool>,
    stop_w: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl TtyEvents {
    pub(crate) fn start() -> Self {
        match start_reader() {
            Ok(events) => events,
            Err(_) => {
                let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
                Self {
                    rx,
                    pending: VecDeque::new(),
                    restore: None,
                }
            }
        }
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<TuiEvent, io::Error>> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        if self.restore.is_none() {
            std::future::pending::<()>().await;
            return None;
        }
        self.rx.recv().await
    }

    pub(crate) async fn wait_for_background(
        &mut self,
        timeout: std::time::Duration,
    ) -> Option<crate::tui::terminal_bg::Rgb> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(Ok(TuiEvent::Background(rgb)))) => return Some(rgb),
                Ok(Some(event @ Ok(_))) => self.pending.push_back(event),
                Ok(Some(event @ Err(_))) => {
                    self.pending.push_back(event);
                    return None;
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }
}

impl Drop for TtyEvents {
    fn drop(&mut self) {
        let Some(mut restore) = self.restore.take() else {
            return;
        };
        restore.stop.store(true, Ordering::Relaxed);
        let _ = write(&restore.stop_w, &[1u8]);
        if let Some(thread) = restore.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_reader() -> io::Result<TtyEvents> {
    let tty = open_tty()?;
    set_nonblock(tty.as_raw_fd())?;
    let (stop_r, stop_w) = pipe().map_err(io_err)?;
    set_nonblock(stop_r.as_raw_fd())?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("lofi-tty-events".into())
        .spawn(move || reader_loop(tty, stop_r, stop_flag, tx))
        .map_err(io::Error::other)?;

    Ok(TtyEvents {
        rx,
        pending: VecDeque::new(),
        restore: Some(ReaderRestore {
            stop,
            stop_w,
            thread: Some(thread),
        }),
    })
}

fn open_tty() -> io::Result<OwnedFd> {
    // F_SETFL acts on the open file description, not one descriptor. A dup of
    // stdin can therefore make terminal output nonblocking when stdin and
    // stdout refer to the same description. Open the controlling terminal
    // independently so the reader's O_NONBLOCK flag stays local.
    let file = File::options().read(true).write(true).open("/dev/tty")?;
    Ok(file.into())
}

fn set_nonblock(fd: std::os::fd::RawFd) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io_err)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io_err)?;
    Ok(())
}

fn io_err(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

fn reader_loop(
    tty: OwnedFd,
    stop_r: OwnedFd,
    stop: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<Result<TuiEvent, io::Error>>,
) {
    let mut parser = Parser::default();
    let mut buf = [0u8; 1024];
    while !stop.load(Ordering::Relaxed) {
        let mut fds = [
            PollFd::new(tty.as_fd(), PollFlags::POLLIN),
            PollFd::new(stop_r.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Err(_) => {
                let _ = tx.send(Err(io::Error::other("tty poll failed")));
                break;
            }
            Ok(_) => {}
        }
        if stop.load(Ordering::Relaxed)
            || fds[1]
                .revents()
                .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP))
        {
            break;
        }
        match read(tty.as_raw_fd(), &mut buf) {
            Ok(0) | Err(Errno::EBADF) => {
                let _ = tx.send(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "tty")));
                break;
            }
            Err(Errno::EINTR | Errno::EAGAIN) => continue,
            Err(err) => {
                let _ = tx.send(Err(io_err(err)));
                break;
            }
            Ok(n) => {
                parser.advance(&buf[..n], n == buf.len());
                for event in parser.drain() {
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct Parser {
    buffer: Vec<u8>,
    ready: VecDeque<TuiEvent>,
}

impl Parser {
    fn advance(&mut self, bytes: &[u8], more: bool) {
        for (idx, byte) in bytes.iter().enumerate() {
            let more = idx + 1 < bytes.len() || more;
            self.buffer.push(*byte);
            match parse::parse_event(&self.buffer, more) {
                Ok(Some(Parsed::Event(ev))) => {
                    self.ready.push_back(TuiEvent::Input(ev));
                    self.buffer.clear();
                }
                Ok(Some(Parsed::ColorSchemeChanged)) => {
                    self.ready.push_back(TuiEvent::ColorSchemeChanged);
                    self.buffer.clear();
                }
                Ok(Some(Parsed::Background(rgb))) => {
                    self.ready.push_back(TuiEvent::Background(rgb));
                    self.buffer.clear();
                }
                Ok(Some(_)) => {
                    self.buffer.clear();
                }
                Ok(None) => {}
                Err(_) => {
                    self.buffer.clear();
                }
            }
        }
    }

    fn drain(&mut self) -> impl Iterator<Item = TuiEvent> + '_ {
        self.ready.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(bytes: &[u8]) -> Vec<TuiEvent> {
        let mut parser = Parser::default();
        parser.advance(bytes, false);
        parser.drain().collect()
    }

    #[test]
    fn parses_color_scheme_change_and_keys() {
        let out = parse_all(b"a\x1b[?997;1nb");
        assert!(matches!(
            &out[..],
            [
                TuiEvent::Input(Event::Key(_)),
                TuiEvent::ColorSchemeChanged,
                TuiEvent::Input(Event::Key(_)),
            ]
        ));
    }

    #[test]
    fn parses_background() {
        let out = parse_all(b"\x1b]11;rgb:ffff/0000/8000\x1b\\");
        assert!(matches!(
            &out[..],
            [TuiEvent::Background(crate::tui::terminal_bg::Rgb {
                r: 255,
                g: 0,
                b: 128,
            })]
        ));
    }

    #[test]
    fn finished_unknown_private_csi_does_not_swallow_keys() {
        let out = parse_all(b"\x1b[?2026;2$yx");
        assert!(matches!(&out[..], [TuiEvent::Input(Event::Key(_))]));
    }

    #[test]
    fn splits_997_across_chunks() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b[?99", true);
        assert!(parser.ready.is_empty());
        parser.advance(b"7;1nX", false);
        let out: Vec<_> = parser.drain().collect();
        assert!(matches!(
            &out[..],
            [TuiEvent::ColorSchemeChanged, TuiEvent::Input(Event::Key(_)),]
        ));
    }
}
