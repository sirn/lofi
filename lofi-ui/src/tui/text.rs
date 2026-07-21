/// Byte offset of the start of row (0-indexed) in s.
pub(super) fn char_is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the start of the word before `cursor` (emacs
/// `backward-word`): skip non-word chars backwards, then word chars.
pub(super) fn prev_word_start(s: &str, cursor: usize) -> usize {
    let chars: Vec<(usize, char)> = s[..cursor].char_indices().collect();
    let mut i = chars.len();
    while i > 0 && !char_is_word(chars[i - 1].1) {
        i -= 1;
    }
    while i > 0 && char_is_word(chars[i - 1].1) {
        i -= 1;
    }
    chars.get(i).map_or(0, |(b, _)| *b)
}

/// Byte offset just past the end of the word at/after `cursor` (emacs
/// `forward-word`): skip non-word forwards, then word chars.
pub(super) fn next_word_end(s: &str, cursor: usize) -> usize {
    let mut byte = cursor;
    let mut in_word = false;
    for (i, c) in s[cursor..].char_indices() {
        let at = cursor + i;
        if char_is_word(c) {
            in_word = true;
            byte = at + c.len_utf8();
        } else if in_word {
            break;
        } else {
            byte = at + c.len_utf8();
        }
    }
    byte
}

/// Compact token/byte count: `9.7M`, `119k`, `500`.
#[allow(clippy::cast_precision_loss)]
pub(super) fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        let v = n as f64 / 1_000_000.0;
        let s = format!("{v:.1}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        format!("{s}M")
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// USD cost with trailing zeros trimmed: `$81.4`, `$81`.
pub(super) fn fmt_cost(c: f64) -> String {
    format!("${c:.2}")
}

/// One component of an abbreviated path: `Dev` -> `D`, `~sirn` -> `~s`.
pub(super) fn abbrev_component(c: &str) -> String {
    if let Some(rest) = c.strip_prefix('~') {
        let head = rest.chars().next().map(|ch| ch.to_string()).unwrap_or_default();
        format!("~{head}")
    } else {
        c.chars().next().map(|ch| ch.to_string()).unwrap_or_default()
    }
}

/// Render a path as `~/...` when under the home dir, then abbreviate to fit
/// `max` display cells: keep the first and last component, shorten the middle
/// to one char each; if that still does not fit, fall back to the basename.
pub(super) fn abbreviate_path(path: &std::path::Path, max: usize) -> String {
    let full: String = match dirs::home_dir() {
        Some(h) if path.starts_with(&h) => match path.strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.to_string_lossy()),
            Err(_) => path.to_string_lossy().to_string(),
        },
        _ => path.to_string_lossy().to_string(),
    };
    let w = |s: &str| unicode_width::UnicodeWidthStr::width(s);
    if w(&full) <= max {
        return full;
    }
    let comps: Vec<&str> = full.split('/').collect();
    let abbr = if comps.len() > 2 {
        let first = comps[0];
        let last = comps[comps.len() - 1];
        let mid: Vec<String> = comps[1..comps.len() - 1]
            .iter()
            .map(|c| abbrev_component(c))
            .collect();
        format!("{}/{}/{}", first, mid.join("/"), last)
    } else {
        full.clone()
    };
    if w(&abbr) <= max {
        return abbr;
    }
    comps.last().copied().unwrap_or("").to_string()
}

pub(super) fn line_start_byte(s: &str, row: usize) -> usize {
    if row == 0 {
        return 0;
    }
    s.char_indices()
        .filter(|(_, c)| *c == '\n')
        .nth(row - 1)
        .map_or(s.len(), |(i, _)| i + 1)
}

/// Convert a char column to bytes within a given logical line of s.
pub(super) fn char_index_to_byte(s: &str, row: usize, col: usize) -> usize {
    let start = line_start_byte(s, row);
    let line_end = s[start..].find('\n').map_or(s.len(), |i| start + i);
    s[start..line_end]
        .char_indices()
        .nth(col)
        .map_or(line_end - start, |(i, _)| i)
}

/// Number of select rows a single logical line occupies when soft-wrapped to
/// `content_w` cells. Mirrors the wrap loop in [`App::input_select_rows`].
pub(super) fn count_wrapped_rows(line: &str, content_w: usize) -> usize {
    if content_w == 0 || line.is_empty() {
        return 1;
    }
    let mut rows = 1usize;
    let mut cur_w = 0usize;
    for c in line.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w + cw > content_w && cur_w > 0 {
            rows += 1;
            cur_w = 0;
        }
        cur_w += cw;
    }
    rows
}

/// Wrap a cursor prefix (the text before the cursor on its logical line) and
/// return `(sub_rows_before, x_on_final_row)`, matching the wrap loop in
/// [`App::input_select_rows`] so the cursor lands exactly where the text
/// would break.
pub(super) fn wrap_prefix_pos(prefix: &str, content_w: usize) -> (usize, usize) {
    if content_w == 0 {
        return (0, unicode_width::UnicodeWidthStr::width(prefix));
    }
    let mut sub = 0usize;
    let mut cur_w = 0usize;
    for c in prefix.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w + cw > content_w && cur_w > 0 {
            sub += 1;
            cur_w = 0;
        }
        cur_w += cw;
    }
    (sub, cur_w)
}

/// Index of the char whose display column is `col` (i.e. the cursor position
/// `col` cells from the left). Never splits a wide char: it lands on the
/// boundary before it.
pub(super) fn col_to_char_idx(s: &str, col: usize) -> usize {
    let mut w = 0usize;
    let mut count = 0usize;
    for c in s.chars() {
        if w >= col {
            return count;
        }
        w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        count += 1;
    }
    count
}
