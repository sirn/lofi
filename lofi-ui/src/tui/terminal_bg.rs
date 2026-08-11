//! Detect the terminal background colour via OSC 11.
//!
//! The query goes out on stdout and the terminal replies on stdin as
//! `ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL` (each component 1–4 hex digits,
//! scaled). The caller MUST engage `crossterm::terminal::enable_raw_mode`
//! before invoking us: cooked mode would echo reply bytes back to the
//! screen and line-buffer stdin, neither of which is compatible with a
//! BEL-terminated response. On any failure (no reply, parse error,
//! terminal that doesn't speak OSC 11) we return `None` and let the
//! caller pick a fallback theme.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

/// A 24-bit RGB triple reported by the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Perceived luminance in [0, 1] using ITU-R BT.601 weights.
    /// Used to decide between the light and dark palette; the threshold
    /// (≈ 0.5) splits "clearly whitish" from "clearly blackish" terminals.
    pub(crate) fn luminance(self) -> f32 {
        let r = f32::from(self.r) / 255.0;
        let g = f32::from(self.g) / 255.0;
        let b = f32::from(self.b) / 255.0;
        0.299 * r + 0.587 * g + 0.114 * b
    }
}

/// Query the terminal's background colour. `timeout` bounds how long we
/// wait for a reply; on time-out or any I/O error the function returns
/// `None` and the caller falls back to the dark palette.
pub(crate) fn query_background(timeout: Duration) -> Option<Rgb> {
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1b]11;?\x07").ok()?;
    stdout.flush().ok()?;

    // Bounded wait via a reader thread + channel. The thread may outlive us
    // on terminals that never reply, but process exit reaps it — acceptable
    // for a ~150 ms query window.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 128];
        let mut n = 0usize;
        let mut chunk = [0u8; 64];
        loop {
            match stdin.read(&mut chunk) {
                Ok(0) | Err(_) => break, // EOF or I/O error
                Ok(m) => {
                    let end = (n + m).min(buf.len());
                    buf[n..end].copy_from_slice(&chunk[..end - n]);
                    n = end;
                    // Response terminator: BEL (\x07) or ST (ESC \\).
                    if chunk[..m].contains(&0x07)
                        || (m >= 2 && chunk[m - 2] == 0x1b && chunk[m - 1] == b'\\')
                    {
                        break;
                    }
                    if n >= buf.len() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(buf[..n].to_vec());
    });
    let bytes = rx.recv_timeout(timeout).ok()?;
    parse_osc11(&bytes)
}

/// Parse an OSC 11 response. The terminal replies with the literal
/// `ESC]11;rgb:RR/GG/BB<terminator>`; each component is 1–4 hex digits.
/// Short (1-digit) components are scaled by replication, per `XParseColor`.
fn parse_osc11(buf: &[u8]) -> Option<Rgb> {
    let mut i = 0;
    while i + 7 < buf.len() {
        if buf[i] == 0x1b
            && buf[i + 1] == b']'
            && buf[i + 2] == b'1'
            && buf[i + 3] == b'1'
            && buf[i + 4] == b';'
            && buf[i + 5] == b'r'
            && buf[i + 6] == b'g'
            && buf[i + 7] == b'b'
            && buf[i + 8] == b':'
        {
            return parse_rgb_tuple(&buf[i + 9..]);
        }
        i += 1;
    }
    None
}

fn parse_rgb_tuple(buf: &[u8]) -> Option<Rgb> {
    let mut parts = [0u16; 3];
    let mut idx = 0usize;
    let mut digits = 0usize;
    let mut value: u16 = 0;
    for &b in buf {
        if b.is_ascii_hexdigit() && digits < 4 {
            value = value
                .saturating_mul(16)
                .saturating_add((b as char).to_digit(16)? as u16);
            digits += 1;
        } else if b == b'/' {
            if idx >= 2 || digits == 0 {
                return None;
            }
            parts[idx] = scale_component(value, digits);
            idx += 1;
            value = 0;
            digits = 0;
        } else {
            // terminator: BEL / ESC / anything else — finalise
            if idx != 2 || digits == 0 {
                return None;
            }
            parts[idx] = scale_component(value, digits);
            return Some(Rgb {
                r: (parts[0] >> 8) as u8,
                g: (parts[1] >> 8) as u8,
                b: (parts[2] >> 8) as u8,
            });
        }
    }
    None
}

/// Scale a hex component to 16-bit per `XParseColor` conventions: 1 and 2
/// digits replicate (`f` -> `ffff`, `ff` -> `ffff`); 3 digits use the
/// libx11 formula (`v << 4 | v >> 8`); 4 digits stay as-is.
fn scale_component(v: u16, digits: usize) -> u16 {
    match digits {
        1 => v * 0x1111,
        2 => v * 0x0101,
        3 => (v << 4) | (v >> 8),
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_response() {
        let buf = b"\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(parse_osc11(buf), Some(Rgb { r: 0, g: 0, b: 0 }));
    }

    #[test]
    fn parses_2digit_hex() {
        let buf = b"\x1b]11;rgb:ff/ff/ff\x07";
        assert_eq!(
            parse_osc11(buf),
            Some(Rgb {
                r: 255,
                g: 255,
                b: 255
            })
        );
    }

    #[test]
    fn parses_with_st_terminator() {
        let buf = b"\x1b]11;rgb:ab/cd/ef\x1b\\";
        assert_eq!(
            parse_osc11(buf),
            Some(Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef
            })
        );
    }

    #[test]
    fn skips_garbage_before_marker() {
        let buf = b"junk\x1b]11;rgb:12/34/56\x07";
        assert_eq!(
            parse_osc11(buf),
            Some(Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            })
        );
    }

    #[test]
    fn rejects_truncated_response() {
        let buf = b"\x1b]11;rgb:ff/ff\x07";
        assert_eq!(parse_osc11(buf), None);
    }

    #[test]
    fn luminance_black_is_zero() {
        assert!(Rgb { r: 0, g: 0, b: 0 }.luminance() < 0.01);
    }

    #[test]
    fn luminance_white_is_one() {
        let l = Rgb {
            r: 255,
            g: 255,
            b: 255,
        }
        .luminance();
        assert!((l - 1.0).abs() < 0.01, "luminance: {l}");
    }

    #[test]
    fn parses_3digit_hex() {
        // libx11 scaling: (v << 4) | (v >> 8). 0xabc -> 0xabca.
        let buf = b"\x1b]11;rgb:a/b/c\x07";
        // 1-digit path: a -> 0xaa, b -> 0xbb, c -> 0xcc.
        assert_eq!(
            parse_osc11(buf),
            Some(Rgb {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc
            })
        );

        // 3-digit path: 0xfff -> (0xfff << 4) | (0xfff >> 8) = 0xff0f | 0xf = 0xffff.
        let buf = b"\x1b]11;rgb:fff/fff/fff\x07";
        assert_eq!(
            parse_osc11(buf),
            Some(Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff
            })
        );
    }
}
