//! TTY input events, including CSI 997 color-scheme reports.
//!
//! Crossterm 0.29 treats a finished unknown `CSI ? …` as incomplete
//! (crossterm#1104) and then swallows later keys. We read inherited stdin
//! and parse it here so 997 is a real event and other private reports are
//! dropped. jjui does the same in bubbletea/ultraviolet.

use std::collections::VecDeque;
use std::io::{self, IsTerminal as _, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::thread::JoinHandle;

use crossterm::event::Event;
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{pipe, read, write};

mod parse;
use parse::Parsed;

/// Terminal color preference from a CSI 997 DSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    Dark,
    Light,
}

const ENABLE_2031: &[u8] = b"\x1b[?2031h";
const DISABLE_2031: &[u8] = b"\x1b[?2031l";
const QUERY_996: &[u8] = b"\x1b[?996n";

pub(crate) fn set_reports_enabled(enabled: bool) {
    let mut out = io::stdout();
    let _ = out.write_all(if enabled { ENABLE_2031 } else { DISABLE_2031 });
    let _ = out.flush();
}

pub(crate) fn request_color_scheme() {
    let mut out = io::stdout();
    let _ = out.write_all(QUERY_996);
    let _ = out.flush();
}

#[derive(Debug)]
pub(crate) enum TuiEvent {
    Input(Event),
    ColorScheme(ColorScheme),
}

pub(crate) struct TtyEvents {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<TuiEvent, io::Error>>,
    stop_w: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl TtyEvents {
    pub(crate) fn start() -> io::Result<Self> {
        let input = io::stdin();
        let (stop_r, stop_w) = pipe().map_err(io_err)?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("lofi-tty-events".into())
            .spawn(move || ReaderTask { input, stop_r, tx }.run())
            .map_err(io::Error::other)?;

        Ok(Self {
            rx,
            stop_w,
            thread: Some(thread),
        })
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<TuiEvent, io::Error>> {
        self.rx.recv().await
    }
}

impl Drop for TtyEvents {
    fn drop(&mut self) {
        let _ = write(&self.stop_w, &[1u8]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn ensure_terminal_input() -> io::Result<()> {
    if io::stdin().is_terminal() {
        Ok(())
    } else {
        Err(io::Error::other(
            "interactive mode requires a terminal on stdin",
        ))
    }
}

fn io_err(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

struct ReaderTask {
    input: io::Stdin,
    stop_r: OwnedFd,
    tx: tokio::sync::mpsc::UnboundedSender<Result<TuiEvent, io::Error>>,
}

impl ReaderTask {
    fn run(self) {
        let Self { input, stop_r, tx } = self;
        let mut parser = Parser::default();
        let mut buf = [0u8; 1024];
        loop {
            let mut fds = [
                PollFd::new(input.as_fd(), PollFlags::POLLIN),
                PollFd::new(stop_r.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(0) | Err(Errno::EINTR) => continue,
                Err(err) => {
                    let _ = tx.send(Err(io_err(err)));
                    break;
                }
                Ok(_) => {}
            }
            if fds[1]
                .revents()
                .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP))
            {
                break;
            }
            match read(input.as_raw_fd(), &mut buf) {
                Ok(0) | Err(Errno::EBADF) => {
                    let _ = tx.send(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin")));
                    break;
                }
                Err(Errno::EINTR | Errno::EAGAIN) => {}
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
                Ok(Some(Parsed::ColorScheme(scheme))) => {
                    self.ready.push_back(TuiEvent::ColorScheme(scheme));
                    self.buffer.clear();
                }
                Ok(Some(_)) | Err(_) => {
                    self.buffer.clear();
                }
                Ok(None) => {}
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
    fn parses_dark_and_keys() {
        let out = parse_all(b"a\x1b[?997;1nb");
        assert!(matches!(
            &out[..],
            [
                TuiEvent::Input(Event::Key(_)),
                TuiEvent::ColorScheme(ColorScheme::Dark),
                TuiEvent::Input(Event::Key(_)),
            ]
        ));
    }

    #[test]
    fn parses_light() {
        let out = parse_all(b"\x1b[?997;2n");
        assert!(matches!(
            &out[..],
            [TuiEvent::ColorScheme(ColorScheme::Light)]
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
            [
                TuiEvent::ColorScheme(ColorScheme::Dark),
                TuiEvent::Input(Event::Key(_)),
            ]
        ));
    }
}
