pub(super) fn char_is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

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

pub(super) fn fmt_cost(c: f64) -> String {
    format!("${c:.2}")
}

pub(super) fn abbrev_component(c: &str) -> String {
    if let Some(rest) = c.strip_prefix('~') {
        let head = rest
            .chars()
            .next()
            .map(|ch| ch.to_string())
            .unwrap_or_default();
        format!("~{head}")
    } else {
        c.chars()
            .next()
            .map(|ch| ch.to_string())
            .unwrap_or_default()
    }
}

/// The path with the home directory shown as ~, without any abbreviation.
/// Every path shown in the UI goes through this so home paths read the
/// same everywhere.
pub(super) fn tilde_path(path: &std::path::Path) -> String {
    match dirs::home_dir() {
        Some(h) if path.starts_with(&h) => match path.strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.to_string_lossy()),
            Err(_) => path.to_string_lossy().to_string(),
        },
        _ => path.to_string_lossy().to_string(),
    }
}

pub(super) fn abbreviate_path(path: &std::path::Path, max: usize) -> String {
    let full: String = tilde_path(path);
    let w = |s: &str| unicode_width::UnicodeWidthStr::width(s);
    if w(&full) <= max {
        return full;
    }
    let comps: Vec<&str> = full.split('/').collect();
    // Abbreviate the components between the first and the last one at a
    // time, from the center outward with the left side first, so the
    // smallest change that fits wins. The leading component follows only
    // once the whole middle is abbreviated.
    if comps.len() > 2 {
        let mut parts: Vec<String> = comps.iter().map(|c| (*c).to_string()).collect();
        for idx in middle_out_order(parts.len() - 2) {
            parts[idx + 1] = abbrev_component(&parts[idx + 1]);
            let candidate = parts.join("/");
            if w(&candidate) <= max {
                return candidate;
            }
        }
        parts[0] = abbrev_component(&parts[0]);
        let candidate = parts.join("/");
        if w(&candidate) <= max {
            return candidate;
        }
    }
    // Nothing whole fits: keep the leading marker and the tail around an
    // ellipsis, then clip the tail to what remains.
    let lead = comps.first().copied().unwrap_or("");
    let tail = comps.last().copied().unwrap_or("");
    if comps.len() > 1 {
        let head = format!("{lead}/…/");
        let head_w = w(&head);
        if max > head_w {
            let budget = max - head_w;
            if w(tail) <= budget {
                return format!("{head}{tail}");
            }
            return format!("{head}{}", clip_marked(tail, budget));
        }
    }
    clip_marked(tail, max)
}

/// Clips the string to the given number of display columns, marking a cut
/// with a trailing ellipsis.
fn clip_marked(s: &str, max: usize) -> String {
    let w = |t: &str| unicode_width::UnicodeWidthStr::width(t);
    if w(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Abbreviation order for the middle components: the center component
/// first, then outward with the left side before the right at equal
/// distance.
fn middle_out_order(n: usize) -> Vec<usize> {
    let center = n / 2;
    let mut order = vec![center];
    for dist in 1..n {
        if center >= dist {
            order.push(center - dist);
        }
        if center + dist < n {
            order.push(center + dist);
        }
    }
    order
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

pub(super) fn char_index_to_byte(s: &str, row: usize, col: usize) -> usize {
    let start = line_start_byte(s, row);
    let line_end = s[start..].find('\n').map_or(s.len(), |i| start + i);
    s[start..line_end]
        .char_indices()
        .nth(col)
        .map_or(line_end - start, |(i, _)| i)
}

/// Word-wrap a single logical line of input to `content_w` display cells,
/// returning the char-index range `[start, end)` of each visual row. This is
/// the shared core for [`App::input_select_rows`], [`count_wrapped_rows`],
/// and [`wrap_cursor_pos`], which must agree so the cursor lands exactly
/// where the text breaks.
pub(super) fn wrap_input_ranges(chars: &[char], content_w: usize) -> Vec<(usize, usize)> {
    let n = chars.len();
    if n == 0 || content_w == 0 {
        return vec![(0, n)];
    }
    let widths: Vec<usize> = chars
        .iter()
        .copied()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .collect();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < n {
        let mut w = 0usize;
        let mut j = i;
        let mut last_space: Option<usize> = None;
        while j < n && w + widths[j] <= content_w {
            if chars[j] == ' ' {
                last_space = Some(j);
            }
            w += widths[j];
            j += 1;
        }
        let end = if j == n {
            n
        } else if let Some(ls) = last_space {
            ls + 1
        } else {
            // No space to break at: hard-break. If even the first char
            // doesn't fit (a wide char in a narrow column), emit it anyway
            // so the loop makes progress — the terminal clips the overflow.
            j.max(i + 1)
        };
        ranges.push((i, end));
        i = end;
    }
    if ranges.is_empty() {
        ranges.push((0, 0));
    }
    ranges
}

/// Number of select rows a single logical line occupies when soft-wrapped.
/// Delegates to [`wrap_input_ranges`] so the count can never drift from the
/// wrap loop the renderer uses.
pub(super) fn count_wrapped_rows(line: &str, content_w: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    wrap_input_ranges(&chars, content_w).len().max(1)
}

/// Map a cursor char column on a logical line to `(select_row, x)` by
/// wrapping the *full* line with the same rules as the renderer. Wrapping
/// the full line (not just the prefix before the cursor) is required
/// because a word boundary can fall past the cursor: the cursor sitting on
/// the first letter of a word that won't fit must already be on the next
/// row, which a prefix-only wrap cannot know.
pub(super) fn wrap_cursor_pos(line: &str, col: usize, content_w: usize) -> (usize, usize) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if content_w == 0 {
        let x = chars
            .iter()
            .copied()
            .take(col)
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        return (0, x);
    }
    let ranges = wrap_input_ranges(&chars, content_w);
    for (sub, &(s, e)) in ranges.iter().enumerate() {
        // `col <= e` (not `<`) so a cursor at a row's right edge stays on
        // that row rather than jumping to the next; the next row begins at
        // `e` and only claims `col > e`.
        if col <= e {
            let end = col.min(n);
            let x: usize = chars[s..end]
                .iter()
                .copied()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            return (sub, x);
        }
    }
    let last = *ranges.last().unwrap_or(&(0, 0));
    let x: usize = chars[last.0..last.1]
        .iter()
        .copied()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    (ranges.len().saturating_sub(1), x)
}

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
