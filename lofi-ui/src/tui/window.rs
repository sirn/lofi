//! Tell the terminal which window and directory it shows: OSC 2 names the
//! window and the directory it holds, OSC 7 reports the directory.
//!
//! A terminal keeps the title the last program set, and a mux keeps the name
//! it took from one, so a launch that writes nothing inherits an outdated
//! title. lofi names the window `lofi <directory name>`: a header shows the
//! title and nothing else, so the directory has to be part of it. Only a space
//! splits the two, so a reader that takes the first word gets the program name
//! alone. The cwd report still goes out: it lets a new tab, window, or pane
//! open where lofi runs. It carries this machine's host name in its authority,
//! as a shell integration does, so a reader can tell the path from one that
//! names another machine.
//!
//! Both sequences go out bare, never inside the tmux passthrough wrapper that
//! the program status needs. tmux knows OSC 2 and OSC 7, so tmux parses them
//! and decides what to do: `set-titles` and `allow-rename` govern the name,
//! and the mux may derive a pane path from the cwd. A wrapped sequence would
//! skip that decision and reach the terminal outside directly.
//!
//! A redirected stdout gets neither sequence: print mode and piped output stay
//! clean, and an escape sequence written to a file is only noise.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

pub(crate) fn announce(cwd: &Path) {
    let mut stdout = io::stdout().lock();
    if !stdout.is_terminal() {
        return;
    }
    let host = hostname();
    let _ = stdout.write_all(sequences(cwd, host.as_deref()).as_bytes());
    let _ = stdout.flush();
}

/// Both sequences in one write: a launch makes one trip to the terminal, and
/// neither sequence can split the other.
fn sequences(cwd: &Path, host: Option<&str>) -> String {
    format!("{}{}", title(cwd), cwd_url(cwd, host))
}

fn title(cwd: &Path) -> String {
    let name = cwd.file_name().unwrap_or(cwd.as_os_str());
    format!("\x1b]2;lofi {}\x1b\\", shown(&name.to_string_lossy()))
}

/// Control characters would end the sequence early.
fn shown(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn cwd_url(cwd: &Path, host: Option<&str>) -> String {
    let host = host.map_or_else(String::new, encoded);
    format!("\x1b]7;file://{host}{}\x1b\\", encoded_path(cwd))
}

/// This machine's name, for the URL authority. A failed lookup leaves the host
/// empty, which the standard reads as the machine that reads the report.
fn hostname() -> Option<String> {
    // POSIX lets a system set its limit anywhere up to 255 bytes.
    let mut buf = [0u8; 256];
    #[allow(unsafe_code)]
    {
        // SAFETY: the call writes at most `buf.len()` bytes into a buffer
        // that outlives it, and it touches no other state.
        let done = unsafe { nix::libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if done != 0 {
            return None;
        }
    }
    let end = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    let host = String::from_utf8_lossy(&buf[..end]).into_owned();
    (!host.is_empty()).then_some(host)
}

fn encoded_path(cwd: &Path) -> String {
    encoded(&cwd.as_os_str().to_string_lossy())
}

/// Only the characters a path may carry unescaped survive as themselves: a
/// space or a `#` inside a URL ends it early.
fn encoded(value: &str) -> String {
    let mut out = String::new();
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'/' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_names_the_program_and_the_directorys_last_part() {
        assert_eq!(
            title(Path::new("/home/user/src/project")),
            "\x1b]2;lofi project\x1b\\"
        );
    }

    #[test]
    fn the_root_keeps_its_whole_text() {
        assert_eq!(title(Path::new("/")), "\x1b]2;lofi /\x1b\\");
    }

    #[test]
    fn a_control_character_never_reaches_the_title() {
        assert_eq!(shown("/tmp/two\nlines\x07"), "/tmp/two lines ");
    }

    #[test]
    fn the_cwd_report_names_the_host_and_the_directory() {
        assert_eq!(
            cwd_url(Path::new("/home/user/src/lofi"), Some("ws-personal")),
            "\x1b]7;file://ws-personal/home/user/src/lofi\x1b\\"
        );
    }

    #[test]
    fn a_host_that_cannot_be_read_still_reports_the_directory() {
        assert_eq!(
            cwd_url(Path::new("/home/user/src/lofi"), None),
            "\x1b]7;file:///home/user/src/lofi\x1b\\"
        );
    }

    #[test]
    fn the_cwd_report_escapes_what_would_end_the_url() {
        assert_eq!(
            encoded_path(Path::new("/tmp/a b#c?d%e")),
            "/tmp/a%20b%23c%3Fd%25e"
        );
        assert_eq!(encoded_path(Path::new("/tmp/héllo")), "/tmp/h%C3%A9llo");
    }

    #[test]
    fn one_write_carries_both_sequences() {
        assert_eq!(
            sequences(Path::new("/home/user/src/project"), Some("ws-personal")),
            "\x1b]2;lofi project\x1b\\\x1b]7;file://ws-personal/home/user/src/project\x1b\\"
        );
    }
}
