//! Parse terminal colours from OSC 11 and OSC 4 replies.

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

/// Parse an OSC 11 response. The terminal replies with the literal
/// `ESC]11;rgb:RR/GG/BB<terminator>`; each component is 1–4 hex digits.
/// Short (1-digit) components are scaled by replication, per `XParseColor`.
pub(crate) fn parse_osc11(buf: &[u8]) -> Option<Rgb> {
    let mut i = 0;
    // Guard the index of the ':' we read below, not the 'b' before it:
    // a chunk that ends exactly at `ESC]11;rgb` must not index past the end.
    while i + 8 < buf.len() {
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

/// Parse one OSC 4 palette-entry reply.
pub(crate) fn parse_osc4(buf: &[u8]) -> Option<(u8, Rgb)> {
    let rest = buf.strip_prefix(b"\x1b]4;")?;
    let separator = rest.iter().position(|byte| *byte == b';')?;
    let index = std::str::from_utf8(&rest[..separator]).ok()?.parse().ok()?;
    let rgb = rest[separator + 1..].strip_prefix(b"rgb:")?;
    Some((index, parse_rgb_tuple(rgb)?))
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
    fn parses_palette_entry_with_scaled_components() {
        assert_eq!(
            parse_osc4(b"\x1b]4;232;rgb:a/12/abc\x1b\\"),
            Some((
                232,
                Rgb {
                    r: 0xaa,
                    g: 0x12,
                    b: 0xab
                }
            )),
        );
        assert_eq!(parse_osc4(b"\x1b]4;256;rgb:f/f/f\x07"), None);
        assert_eq!(parse_osc4(b"\x1b]4;16;rgb:ff/ff"), None);
    }

    #[test]
    fn parses_canonical_response() {
        let buf = b"\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(parse_osc11(buf), Some(Rgb { r: 0, g: 0, b: 0 }));
    }

    #[test]
    fn truncated_prefix_is_not_a_panic() {
        assert_eq!(parse_osc11(b"\x1b]11;rgb"), None);
        assert_eq!(parse_osc11(b"\x1b]11;rgb:"), None);
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
