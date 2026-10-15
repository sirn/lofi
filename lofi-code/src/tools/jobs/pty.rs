//! PTY and terminal-screen ownership for tty jobs: the pseudo-terminal
//! driver pieces, the vt100 screen model behind the job viewer, and idle
//! tracking for interactive-prompt detection.

use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use lofi_error::{Error, Result};

use super::{ensure_job_running, JobHandle};

/// A terminal color captured independently of the UI renderer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JobColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Text attributes captured for terminal cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobAttributes(u8);

impl JobAttributes {
    pub const BOLD: Self = Self(1 << 0);
    pub const DIM: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const UNDERLINE: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for JobAttributes {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for JobAttributes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Display attributes shared by one or more adjacent terminal cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobStyle {
    pub foreground: JobColor,
    pub background: JobColor,
    pub attributes: JobAttributes,
}

/// Adjacent terminal cells with the same display attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobSpan {
    pub text: String,
    pub style: JobStyle,
}

/// One row of a parsed terminal screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobScreenLine {
    pub spans: Vec<JobSpan>,
}

/// The visible screen of a PTY job after terminal escape sequences and
/// display attributes have been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobScreen {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<JobScreenLine>,
}

/// Allocate a pseudo-terminal with the requested window size and put the
/// master in non-blocking mode so the driver poll never blocks on a silent
/// child.
pub(super) fn open_pty(cols: u16, rows: u16) -> std::io::Result<nix::pty::OpenptyResult> {
    let winsize = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None::<&nix::sys::termios::Termios>)
        .map_err(std::io::Error::from)?;
    set_nonblock(pty.master.as_raw_fd())?;
    Ok(pty)
}

fn set_nonblock(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let flags =
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).map_err(std::io::Error::from)?;
    let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags)).map_err(std::io::Error::from)?;
    Ok(())
}

/// Map a named key to the byte sequence an xterm sends for it.
pub(super) fn key_press_bytes(key: &str, application_cursor: bool) -> Option<&'static [u8]> {
    let bytes: &[u8] = match key {
        "Enter" | "Return" => b"\r",
        "Tab" => b"\t",
        "Escape" | "Esc" => b"\x1b",
        "Backspace" => b"\x7f",
        "Delete" => b"\x1b[3~",
        "Insert" => b"\x1b[2~",
        "Up" if application_cursor => b"\x1bOA",
        "Down" if application_cursor => b"\x1bOB",
        "Right" if application_cursor => b"\x1bOC",
        "Left" if application_cursor => b"\x1bOD",
        "Home" if application_cursor => b"\x1bOH",
        "End" if application_cursor => b"\x1bOF",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        "PageUp" => b"\x1b[5~",
        "PageDown" => b"\x1b[6~",
        "Space" => b" ",
        "Ctrl+C" => b"\x03",
        "Ctrl+D" => b"\x04",
        "Ctrl+Z" => b"\x1a",
        "Ctrl+U" => b"\x15",
        "Ctrl+L" => b"\x0c",
        "Ctrl+A" => b"\x01",
        "Ctrl+E" => b"\x05",
        "F1" => b"\x1bOP",
        "F2" => b"\x1bOQ",
        "F3" => b"\x1bOR",
        "F4" => b"\x1bOS",
        "F5" => b"\x1b[15~",
        "F6" => b"\x1b[17~",
        "F7" => b"\x1b[18~",
        "F8" => b"\x1b[19~",
        "F9" => b"\x1b[20~",
        "F10" => b"\x1b[21~",
        "F11" => b"\x1b[23~",
        "F12" => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes)
}

pub(super) fn write_to_master(handle: &JobHandle, bytes: &[u8]) -> Result<usize> {
    ensure_job_running(handle)?;
    let guard = handle
        .master
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(master) = guard.as_ref() else {
        return Err(Error::Tool("job is not a tty job".into()));
    };
    let mut written = 0;
    while written < bytes.len() {
        match nix::unistd::write(master.as_fd(), &bytes[written..]) {
            Ok(0) => return Ok(written),
            Ok(n) => written += n,
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                if written > 0 {
                    return Ok(written);
                }
                return Err(Error::Tool("pty input buffer full; retry".into()));
            }
            Err(e) => return Err(Error::Io(std::io::Error::from(e))),
        }
    }
    Ok(written)
}

/// Drain whatever the PTY master has ready, appending raw bytes to the log
/// and feeding the same bytes to the screen parser. Returns without blocking;
/// EAGAIN/EWOULDBLOCK means no more data, EIO means the slave side closed.
pub(super) fn drain_master(
    master: &OwnedFd,
    parser: &mut Option<vt100::Parser>,
    log_out: &mut Option<std::fs::File>,
) -> std::io::Result<bool> {
    let mut buf = [0u8; 8192];
    let mut changed = false;
    loop {
        match nix::unistd::read(master.as_raw_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                changed = true;
                if let Some(out) = log_out.as_mut() {
                    out.write_all(&buf[..n])?;
                }
                if let Some(parser) = parser.as_mut() {
                    parser.process(&buf[..n]);
                }
            }
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                break;
            }
            Err(nix::errno::Errno::EIO) => break,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
    Ok(changed)
}

fn job_color(color: vt100::Color) -> JobColor {
    match color {
        vt100::Color::Default => JobColor::Default,
        vt100::Color::Idx(index) => JobColor::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => JobColor::Rgb(red, green, blue),
    }
}

fn job_style(cell: &vt100::Cell) -> JobStyle {
    let mut attributes = JobAttributes::default();
    if cell.bold() {
        attributes |= JobAttributes::BOLD;
    }
    if cell.dim() {
        attributes |= JobAttributes::DIM;
    }
    if cell.italic() {
        attributes |= JobAttributes::ITALIC;
    }
    if cell.underline() {
        attributes |= JobAttributes::UNDERLINE;
    }
    if cell.inverse() {
        attributes |= JobAttributes::INVERSE;
    }
    JobStyle {
        foreground: job_color(cell.fgcolor()),
        background: job_color(cell.bgcolor()),
        attributes,
    }
}

pub(super) fn publish_screen(handle: &JobHandle, parser: &vt100::Parser) {
    let screen = parser.screen();
    handle.application_cursor.store(
        screen.application_cursor(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let (rows, cols) = screen.size();
    let lines = (0..rows)
        .map(|row| {
            let mut spans: Vec<JobSpan> = Vec::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let style = job_style(cell);
                let span = match spans.last_mut() {
                    Some(span) if span.style == style => span,
                    _ => {
                        spans.push(JobSpan {
                            text: String::new(),
                            style,
                        });
                        let index = spans.len() - 1;
                        &mut spans[index]
                    }
                };
                if cell.has_contents() {
                    span.text.push_str(cell.contents());
                } else {
                    span.text.push(' ');
                }
            }
            JobScreenLine { spans }
        })
        .collect();
    *handle
        .screen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(JobScreen { cols, rows, lines });
}

pub(super) fn drain_pty(
    handle: &JobHandle,
    master: &OwnedFd,
    parser: &mut Option<vt100::Parser>,
    log_out: &mut Option<std::fs::File>,
) {
    if drain_master(master, parser, log_out).unwrap_or(false) {
        if let Some(parser) = parser {
            publish_screen(handle, parser);
        }
    }
}

pub(super) fn screen_changed(parser: &vt100::Parser, last: &mut Option<String>) -> bool {
    let contents = parser.screen().contents();
    if last.as_deref() == Some(contents.as_str()) {
        return false;
    }
    *last = Some(contents);
    true
}

/// Output-idle bookkeeping for one running job. Screen text is the change
/// signal for a tty job (so cursor movement or redraws do not count as work);
/// log byte count is the signal for a plain job.
pub(super) struct IdleTracker {
    last_bytes: u64,
    last_screen: Option<String>,
    last_output_at: Instant,
    idle: bool,
}

impl IdleTracker {
    pub(super) fn new() -> Self {
        Self {
            last_bytes: 0,
            last_screen: None,
            last_output_at: Instant::now(),
            idle: false,
        }
    }
}

/// Update activity timestamps from the current output. Returns true when the
/// output changed since the previous poll. `job.last_output_at` is always
/// tracked even when idle notices are disabled.
pub(super) fn update_activity(
    handle: &JobHandle,
    parser: Option<&vt100::Parser>,
    log_path: &str,
    tracker: &mut IdleTracker,
) -> bool {
    let changed = if let Some(parser) = parser {
        screen_changed(parser, &mut tracker.last_screen)
    } else {
        let bytes = std::fs::metadata(log_path).map_or(0, |m| m.len());
        if bytes == tracker.last_bytes {
            false
        } else {
            tracker.last_bytes = bytes;
            true
        }
    };
    if changed {
        let now = Instant::now();
        tracker.last_output_at = now;
        tracker.idle = false;
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.last_output_at = Some(now);
        job.idle = false;
    }
    changed
}

/// Emit one edge-triggered idle notice when output has been still for the
/// configured threshold. Returns `None` while active or before the threshold.
pub(super) fn observe_idle(
    handle: &JobHandle,
    parser: Option<&vt100::Parser>,
    log_path: &str,
    tracker: &mut IdleTracker,
    bash_env: &crate::BashEnv,
) -> Option<String> {
    if update_activity(handle, parser, log_path, tracker) {
        return None;
    }
    let (idle_ms, notify_enabled) = {
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (job.idle_ms, job.notify.enabled)
    };
    let idle_ms = idle_ms?;
    let now = Instant::now();
    if tracker.idle || now.duration_since(tracker.last_output_at) < Duration::from_millis(idle_ms) {
        return None;
    }
    tracker.idle = true;
    {
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.idle = true;
    }
    if !notify_enabled {
        return None;
    }
    let tail = idle_tail(parser, log_path, bash_env);
    let id = handle
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .id;
    Some(format!("job {id} idle (no output for {idle_ms}ms): {tail}"))
}

pub(super) fn idle_tail(
    parser: Option<&vt100::Parser>,
    log_path: &str,
    bash_env: &crate::BashEnv,
) -> String {
    if let Some(parser) = parser {
        let contents = parser.screen().contents();
        let mut tail = contents.lines().last().unwrap_or("").trim_end().to_owned();
        bash_env.redact(&mut tail);
        return short_text(&tail, 80);
    }
    let tail = read_redacted_log_tail(log_path, 200, bash_env);
    short_text(tail.trim(), 80)
}

pub(super) fn read_log_tail(path: &str, max: usize) -> String {
    use std::io::{Read as _, Seek as _};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(total) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let start = total.saturating_sub(max as u64);
    if file.seek(std::io::SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity((total - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(super) fn read_redacted_log_tail(path: &str, max: usize, bash_env: &crate::BashEnv) -> String {
    let redact_overlap = bash_env.redact.iter().map(String::len).max().unwrap_or(0);
    let mut tail = read_log_tail(path, max.saturating_add(redact_overlap));
    bash_env.redact(&mut tail);
    tail
}

/// Collapse control characters to spaces and cap the visible length at a
/// char boundary for one-line notices.
pub(super) fn short_text(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect();
    if s.chars().count() > max {
        out.push('\u{2026}');
    }
    out
}
