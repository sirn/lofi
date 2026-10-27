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
use std::time::{Duration, Instant};

use crossterm::event::Event;
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{pipe, read, write};

mod parse;
use parse::Parsed;

use super::theme::{ColorScheme, ThemeQueries};

const ENABLE_2031: &[u8] = b"\x1b[?2031h";
const DISABLE_2031: &[u8] = b"\x1b[?2031l";
const QUERY_996: &[u8] = b"\x1b[?996n";
const QUERY_OSC11: &[u8] = b"\x1b]11;?\x07";

pub(crate) fn apply_theme_queries(queries: ThemeQueries) {
    if let Some(enabled) = queries.reports {
        set_reports_enabled(enabled);
    }
    if queries.palette {
        request_palette();
    }
    if queries.appearance {
        request_background();
        request_color_scheme();
    }
}

pub(crate) fn set_reports_enabled(enabled: bool) {
    let mut out = io::stdout();
    let _ = out.write_all(if enabled { ENABLE_2031 } else { DISABLE_2031 });
    let _ = out.flush();
}

/// Ask for the background colour. A multiplexer only forwards this to a
/// terminal that is attached right now, so there is no window to miss: the
/// reply arrives as `TuiEvent::Background` however late it is.
fn request_background() {
    let mut out = io::stdout();
    let _ = out.write_all(QUERY_OSC11);
    let _ = out.flush();
}

fn request_color_scheme() {
    let mut out = io::stdout();
    let _ = out.write_all(QUERY_996);
    let _ = out.flush();
}

/// Query each endpoint separately because terminals can reply in any order.
fn request_palette() {
    let mut out = io::stdout();
    for index in [232, 255, 16, 231] {
        let _ = write!(out, "\x1b]4;{index};?\x07");
    }
    let _ = out.flush();
}

#[derive(Debug)]
pub(crate) enum TuiEvent {
    Input(Event),
    ColorScheme(ColorScheme),
    /// Answer to an OSC 11 background-colour query.
    Background(crate::tui::terminal_bg::Rgb),
    Palette(u8, crate::tui::terminal_bg::Rgb),
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
            parser.expire_escape(Instant::now());
            for event in parser.drain() {
                if tx.send(Ok(event)).is_err() {
                    return;
                }
            }
            let mut fds = [
                PollFd::new(input.as_fd(), PollFlags::POLLIN),
                PollFd::new(stop_r.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, parser.poll_timeout()) {
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
                    parser.advance(&buf[..n]);
                }
            }
        }
    }
}

const ESCAPE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Parser {
    buffer: Vec<u8>,
    escape_deadline: Option<Instant>,
    ready: VecDeque<TuiEvent>,
}

impl Parser {
    fn advance(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.buffer.push(*byte);
            match parse::parse_event(&self.buffer, true) {
                Ok(Some(Parsed::Event(ev))) => {
                    self.ready.push_back(TuiEvent::Input(ev));
                    self.buffer.clear();
                }
                Ok(Some(Parsed::ColorScheme(scheme))) => {
                    self.ready.push_back(TuiEvent::ColorScheme(scheme));
                    self.buffer.clear();
                }
                Ok(Some(Parsed::Background(rgb))) => {
                    self.ready.push_back(TuiEvent::Background(rgb));
                    self.buffer.clear();
                }
                Ok(Some(Parsed::Palette(index, rgb))) => {
                    self.ready.push_back(TuiEvent::Palette(index, rgb));
                    self.buffer.clear();
                }
                Ok(Some(_)) | Err(_) => {
                    self.buffer.clear();
                }
                Ok(None) => {}
            }
        }
        // A read boundary is not an Escape key: SSH can split a reply here.
        self.escape_deadline = (self.buffer == b"\x1b").then(|| Instant::now() + ESCAPE_TIMEOUT);
    }

    fn poll_timeout(&self) -> PollTimeout {
        self.escape_deadline.map_or(PollTimeout::NONE, |deadline| {
            PollTimeout::try_from(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(PollTimeout::MAX)
        })
    }

    fn expire_escape(&mut self, now: Instant) {
        if self.escape_deadline.is_some_and(|deadline| now >= deadline) {
            self.escape_deadline = None;
            self.buffer.clear();
            self.ready.push_back(TuiEvent::Input(Event::Key(
                crossterm::event::KeyCode::Esc.into(),
            )));
        }
    }

    fn drain(&mut self) -> impl Iterator<Item = TuiEvent> + '_ {
        self.ready.drain(..)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse_all(bytes: &[u8]) -> Vec<TuiEvent> {
        let mut parser = Parser::default();
        parser.advance(bytes);
        parser.drain().collect()
    }

    #[test]
    fn palette_replies_survive_every_read_boundary() {
        for reply in [
            b"\x1b]4;232;rgb:ffff/eeee/dddd\x07".as_slice(),
            b"\x1b]4;255;rgb:0/12/345\x1b\\".as_slice(),
        ] {
            for split in 1..reply.len() {
                let mut parser = Parser::default();
                parser.advance(&reply[..split]);
                parser.advance(&reply[split..]);
                let out: Vec<_> = parser.drain().collect();
                assert!(
                    matches!(&out[..], [TuiEvent::Palette(_, _)]),
                    "{split}: {out:?}"
                );
            }
        }
        let mut parser = Parser::default();
        for byte in b"a\x1b]4;16;rgb:ffff/ffff/ffff\x07\x1b]4;231;rgb:0/0/0\x1b\\b" {
            parser.advance(&[*byte]);
        }
        let out: Vec<_> = parser.drain().collect();
        assert_eq!(typed_chars(&out), "ab");
        assert!(matches!(
            &out[1..3],
            [TuiEvent::Palette(16, _), TuiEvent::Palette(231, _)]
        ));
    }

    #[test]
    fn malformed_palette_replies_do_not_become_input() {
        for reply in [
            b"\x1b]4;256;rgb:ff/ff/ff\x07x".as_slice(),
            b"\x1b]4;16;rgb:ff/ff\x07x".as_slice(),
            b"\x1b]4;16;?\x07x".as_slice(),
        ] {
            let out = parse_all(reply);
            assert_eq!(typed_chars(&out), "x");
            assert_eq!(out.len(), 1);
        }
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

    fn typed_chars(out: &[TuiEvent]) -> String {
        out.iter()
            .filter_map(|event| match event {
                TuiEvent::Input(Event::Key(key)) => match key.code {
                    crossterm::event::KeyCode::Char(ch) => Some(ch),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn drops_bel_terminated_osc_reply() {
        let out = parse_all(b"a\x1b]11;rgb:ffff/ffff/ffff\x07b");
        assert_eq!(typed_chars(&out), "ab");
    }

    #[test]
    fn osc11_reply_reports_background() {
        let out = parse_all(b"\x1b]11;rgb:ffff/ffff/ffff\x07");
        assert!(matches!(
            &out[..],
            [TuiEvent::Background(crate::tui::terminal_bg::Rgb {
                r: 255,
                g: 255,
                b: 255,
            })]
        ));
    }

    #[test]
    fn drops_st_terminated_osc_reply() {
        let out = parse_all(b"\x1b]10;rgb:0000/0000/0000\x1b\\a");
        assert_eq!(typed_chars(&out), "a");
    }

    #[test]
    fn splits_osc_reply_across_chunks() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b]11;rgb:ff");
        assert!(parser.ready.is_empty());
        parser.advance(b"ff/ffff/ffff\x07X");
        let out: Vec<_> = parser.drain().collect();
        assert_eq!(typed_chars(&out), "X");
    }

    #[test]
    fn lone_escape_is_emitted_only_after_timeout() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b");
        let deadline = parser.escape_deadline.unwrap();
        parser.expire_escape(deadline.checked_sub(Duration::from_millis(1)).unwrap());
        assert!(parser.ready.is_empty());
        parser.expire_escape(deadline);
        let out: Vec<_> = parser.drain().collect();
        assert!(matches!(
            &out[..],
            [TuiEvent::Input(Event::Key(key))]
                if key.code == crossterm::event::KeyCode::Esc
        ));
        assert!(parser.buffer.is_empty());
        assert!(parser.escape_deadline.is_none());
    }

    #[test]
    fn reply_continuation_cancels_escape_timeout() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b");
        let deadline = parser.escape_deadline.unwrap();
        parser.advance(b"[?997;2n");
        parser.expire_escape(deadline);
        let out: Vec<_> = parser.drain().collect();
        assert!(matches!(
            &out[..],
            [TuiEvent::ColorScheme(ColorScheme::Light)]
        ));
    }

    #[test]
    fn theme_replies_survive_byte_at_a_time_reads() {
        let mut parser = Parser::default();
        for byte in b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?997;2n" {
            parser.advance(&[*byte]);
        }
        let out: Vec<_> = parser.drain().collect();
        assert!(matches!(
            &out[..],
            [
                TuiEvent::Background(_),
                TuiEvent::ColorScheme(ColorScheme::Light)
            ]
        ));
    }

    #[test]
    fn osc11_survives_every_read_boundary() {
        let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        for split in 1..reply.len() {
            let mut parser = Parser::default();
            parser.advance(&reply[..split]);
            parser.advance(&reply[split..]);
            let out: Vec<_> = parser.drain().collect();
            assert!(
                matches!(&out[..], [TuiEvent::Background(_)]),
                "split at {split}: {out:?}",
            );
        }
    }

    #[test]
    fn color_scheme_survives_every_read_boundary() {
        let reply = b"\x1b[?997;2n";
        for split in 1..reply.len() {
            let mut parser = Parser::default();
            parser.advance(&reply[..split]);
            parser.advance(&reply[split..]);
            let out: Vec<_> = parser.drain().collect();
            assert!(
                matches!(&out[..], [TuiEvent::ColorScheme(ColorScheme::Light)]),
                "split at {split}: {out:?}",
            );
        }
    }

    #[test]
    fn splits_997_across_chunks() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b[?99");
        assert!(parser.ready.is_empty());
        parser.advance(b"7;1nX");
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
