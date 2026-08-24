            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "read" | "view" | "bash_read" => {
            let start = n("start_line").max(1);
            let total = n("total_lines");
            let lines = split_lines(s("content"));
            let notice = b("truncated").then_some(format!(
                "(showing {start}-{} of {total}; use offset={next} to continue)",
                start + lines.len().saturating_sub(1),
                next = start + lines.len()
            ));
            NativeBody {
                lines,
                numbered: true,
                start_line: start,
                is_diff: false,
                notice,
            }
        }
        "ls" => {
            let lines = v.get("entries").and_then(|x| x.as_array()).map_or_else(
                || split_lines(raw),
                |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                },
            );
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "find" => {
            let lines = v.get("matches").and_then(|x| x.as_array()).map_or_else(
                || split_lines(raw),
                |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                },
            );
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "agent" => {
            let text = s("text");
            let model = s("model");
            let thinking = s("thinking");
            let rounds = n("rounds");
            let duration_ms = n("durationMs");
            let cost = v
                .get("cost")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            let notice = (!model.is_empty()).then(|| {
                format!(
                    "({model}, {thinking}, {rounds} rounds, {}, ${cost:.4})",
                    prim::fmt_duration(std::time::Duration::from_millis(duration_ms as u64))
                )
            });
            NativeBody {
                lines: split_lines(text),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice,
            }
        }
        "grep" => {
            let mut lines = Vec::new();
            if let Some(arr) = v.get("matches").and_then(|x| x.as_array()) {
                for m in arr {
                    let file = m.get("file").and_then(|x| x.as_str()).unwrap_or("");
                    let line = m
                        .get("line")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let content = m.get("content").and_then(|x| x.as_str()).unwrap_or("");
                    lines.push(format!("{file}:{line}:{content}"));
                }
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "jobRead" => {
            let output = s("output");
            let cursor = v
                .get("cursor")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let total = v
                .get("totalBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let done = b("done");
            let notice = if done {
                Some(format!("(end of log; {total} bytes total)"))
            } else if total > 0 {
                Some(format!("(read {cursor} of {total} bytes)"))
            } else {
                None
            };
            NativeBody {
                lines: split_lines(output),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice,
            }
        }
        "jobSpawn" | "jobStatus" | "jobWait" | "jobKill" | "jobNotify" => {
            let mut lines: Vec<String> = Vec::new();
            let state = s("state");
            if !state.is_empty() {
                lines.push(format!("state: {state}"));
            }
            let mut stats: Vec<String> = Vec::new();
            if let Some(code) = v.get("exitCode").and_then(serde_json::Value::as_i64) {
                stats.push(format!("exit {code}"));
            }
            if let Some(sig) = v.get("signal").and_then(serde_json::Value::as_i64) {
                stats.push(format!("signal {sig}"));
            }
            if let Some(ms) = v.get("durationMs").and_then(serde_json::Value::as_u64) {
                stats.push(format!(
                    "duration {}",
                    prim::fmt_duration(std::time::Duration::from_millis(ms))
                ));
            }
            if !stats.is_empty() {
                lines.push(stats.join("  "));
            }
            let mut notify_bits: Vec<String> = Vec::new();
            if v.get("notify").is_some() {
                notify_bits.push(format!("notify: {}", b("notify")));
            }
            if let Some(iv) = v
                .get("notifyIntervalMs")
                .and_then(serde_json::Value::as_u64)
            {
                notify_bits.push(format!(
                    "interval {}",
                    prim::fmt_duration(std::time::Duration::from_millis(iv))
                ));
            }
            if v.get("notifyChanged").is_some() {
                notify_bits.push(format!("changed {}", b("notifyChanged")));
            }
            if !notify_bits.is_empty() {
                lines.push(notify_bits.join("  "));
            }
            let log_path = s("logPath");
            if !log_path.is_empty() {
                lines.push(format!("log: {log_path}"));
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        _ => NativeBody {
            lines: split_lines(raw),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
    }
}

pub(crate) fn compact_native_previews(turn: &mut Turn) {
    for block in &mut turn.blocks {
        let Block::Tool(tool) = block else { continue };
        for native in &mut tool.native {
            if native.is_error || native.result.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            if !matches!(native.name.as_str(), "bash" | "write" | "edit" | "agent") {
                native.result = None;
                continue;
            }
            let raw = native.result.as_deref().unwrap_or_default();
            let header_suffix = native_header_suffix(&native.name, Some(raw));
            let encoded_field = match native.name.as_str() {
                "bash" => Some("output"),
                "write" => Some("content"),
                _ => None,
            };
            let encoded = encoded_field.and_then(|field| json_string_field(raw, field));
            let body = encoded.is_none().then(|| native_body(native));
            let total_lines = encoded.map_or_else(
                || body.as_ref().map_or(0, |body| body.lines.len()),
                encoded_json_line_count,
            );
            let range = native_preview_range(&native.name, total_lines, false);
            let mut lines = Vec::with_capacity(range.len());
            if let Some(encoded) = encoded {
                for_each_encoded_json_line(encoded, range.clone(), |_, line| lines.push(line));
            } else if let Some(body) = &body {
                lines.extend(body.lines[range.clone()].iter().cloned());
            }
            native.preview = Some(Box::new(NativePreview {
                header_suffix,
                lines,
                total_lines,
                preview_start: range.start,
                numbered: body.as_ref().is_some_and(|body| body.numbered),
                start_line: body.as_ref().map_or(1, |body| body.start_line),
                is_diff: body.as_ref().is_some_and(|body| body.is_diff),
                notice: body.and_then(|body| body.notice),
            }));
            native.result = None;
        }
    }
}

fn native_preview_range(name: &str, total: usize, verbose: bool) -> std::ops::Range<usize> {
    if verbose {
        return 0..total;
    }
    let shown = total.min(PREVIEW_LINES);
    let start = if name == "bash" {
        total.saturating_sub(shown)
    } else {
        0
    };
    start..start + shown
}

fn split_lines(s: &str) -> Vec<String> {
    s.trim_end_matches('\n')
        .split('\n')
        .map(String::from)
        .collect()
}

/// Collapse JSON tool args into a short header label for known tools so
/// the `Tool <name> <args>` line scans like a sentence instead of dumping
/// the request envelope. Returns `None` for tools we have no tailored
/// view for; the caller falls back to the verbatim args.
///
/// Only shapes the surface actually emits are recognised (every job op
/// takes an `id`; `jobSpawn` takes a command). Anything else returns
/// `None`.
fn summarize_tool_args(name: &str, args: &str) -> Option<String> {
    if args.is_empty() {
        return Some(String::new());
    }
    let id = json_string_field(args, "id");
    match name {
        "jobSpawn" => json_string_field(args, "cmd").map(|cmd| truncate_args_display(cmd, 60)),
        "jobStatus" | "jobRead" | "jobWait" | "jobKill" | "jobNotify" => {
            id.map(|s| format!("job {s}"))
        }
        _ => None,
    }
}

/// Cap an args-rendered text cell at `width` chars (multi-byte safe),
/// replacing the overflow with U+2026.
fn truncate_args_display(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
    s.push('\u{2026}');
    s
}

fn json_string_field<'a>(raw: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let bytes = raw.as_bytes();
    let mut i = key + needle.len();
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = i.saturating_add(2),
            b'"' => return Some(&raw[start..i]),
            _ => i += 1,
        }
    }
    None
}

fn json_u64_field(raw: &str, field: &str) -> Option<u64> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let mut rest = raw.get(key + needle.len()..)?.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let end = rest.bytes().take_while(u8::is_ascii_digit).count();
    (end > 0).then(|| rest[..end].parse().ok()).flatten()
}

fn encoded_json_line_count(encoded: &str) -> usize {
    let bytes = encoded.as_bytes();
    let mut i = 0usize;
    let mut breaks = 0usize;
    let mut trailing = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'n' {
                breaks += 1;
                trailing += 1;
            } else {
                trailing = 0;
            }
            i += 2;
        } else {
            trailing = 0;
            i += 1;
        }
    }
    breaks.saturating_add(1).saturating_sub(trailing).max(1)
}

fn for_each_encoded_json_line(
    encoded: &str,
    range: std::ops::Range<usize>,
    mut f: impl FnMut(usize, String),
) {
    let wanted_end = range.end.min(encoded_json_line_count(encoded));
    if range.start >= wanted_end {
        return;
    }
    let bytes = encoded.as_bytes();
    let mut start = 0usize;
    let mut line = 0usize;
    let mut i = 0usize;
    while i <= bytes.len() && line < wanted_end {
        let at_break =
            i == bytes.len() || (bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'n');
        if at_break {
            if line >= range.start {
                let quoted = format!("\"{}\"", &encoded[start..i]);
                if let Ok(decoded) = serde_json::from_str::<String>(&quoted) {
                    f(line, decoded);
                }
            }
            line += 1;
            if i == bytes.len() {
                break;
            }
            i += 2;
            start = i;
        } else if bytes[i] == b'\\' {
            i = i.saturating_add(2);
        } else {
            i += 1;
        }
    }
}

fn native_header_suffix(name: &str, result: Option<&str>) -> Option<String> {
    let raw = result?;
    if raw.is_empty() {
        return None;
    }
    if matches!(name, "read" | "view" | "bash_read") {
        let start = json_u64_field(raw, "start_line")?.max(1);
        let total = json_u64_field(raw, "total_lines")?;
        let shown = json_string_field(raw, "content").map(encoded_json_line_count)? as u64;
        let end = start.saturating_add(shown.saturating_sub(1)).min(total);
        return Some(format!("(lines {start}-{end})"));
    }
    if name == "bash" {
        if let Some(ms) = json_u64_field(raw, "duration_ms") {
            return Some(format!(
                "(took {})",
                prim::fmt_duration(Duration::from_millis(ms))
            ));
        }
    }
    if name == "agent" {
        let model = json_string_field(raw, "model")?;
        let thinking = json_string_field(raw, "thinking").unwrap_or("off");
        let rounds = json_u64_field(raw, "rounds").unwrap_or(0);
        let duration = json_u64_field(raw, "durationMs").unwrap_or(0);
        return Some(format!(
            "({model}:{thinking}, {rounds} rounds, {})",
            prim::fmt_duration(Duration::from_millis(duration))
        ));
    }
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    match name {
        "read" | "view" | "bash_read" => {
            let start = v.get("start_line").and_then(serde_json::Value::as_u64)?;
            if start == 0 {
                return None;
            }
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let count = content.split('\n').count();
            if count == 0 {
                return None;
            }
            let end = start + count as u64 - 1;
            if end <= start {
                return None;
            }
            Some(format!("(lines {start}-{end})"))
        }
        "bash" => {
            let ms = v.get("duration_ms").and_then(serde_json::Value::as_u64)?;
            if ms == 0 {
                return None;
            }
            let dur = std::time::Duration::from_millis(ms);
            Some(format!("(took {})", prim::fmt_duration(dur)))
        }
        _ => None,
    }
}

fn edit_diff(old: &str, new: &str) -> Vec<String> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        let val = change.value();
        let line = val.strip_suffix('\n').unwrap_or(val);
        out.push(format!("{prefix}{line}"));
    }
    out
}

impl Component for ExecBlockBranch<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        self.render_window(cx, 0..usize::MAX).lines
    }

    fn height(&self, cx: &Cx) -> usize {
        self.render_window(cx, 0..0).total
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        self.render_window(cx, range).lines
    }
}

impl ExecBlockBranch<'_> {
    fn render_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> RenderWindow {
        let t = cx.theme;
        let working = !self.nt.done && cx.active_turn;
        let exec_cont = if self.is_last { "  " } else { "│ " };
        let key = DetailKey::NativeTool {
            parent: self.parent.to_string(),
            id: self.nt.id,
        };
        let expanded = cx.app.expanded_details.contains_key(&key);
        let body = self
            .nt
            .result
            .as_deref()
            .filter(|result| !result.is_empty())
            .map(|_| native_body(self.nt));
        let mut lines = body.as_ref().map_or_else(
            || {
                self.nt
                    .preview
                    .as_ref()
                    .map_or_else(Vec::new, |preview| preview.lines.clone())
            },
            |body| body.lines.clone(),
        );
        if let Some(body) = &body {
            if body.numbered {
                for (index, line) in lines.iter_mut().enumerate() {
                    *line = format!("{:>4} {line}", body.start_line + index);
                }
            }
        }
        let total = body.as_ref().map_or_else(
            || {
                self.nt
                    .preview
                    .as_ref()
                    .map_or(lines.len(), |preview| preview.total_lines)
            },
            |body| body.lines.len(),
        );

        let mut content = vec![
            Span::styled("Tool ", Style::new().fg(t.muted)),
            Span::styled(self.nt.name.clone(), Style::new().fg(t.info)),
        ];
        let args_label = summarize_tool_args(&self.nt.name, &self.nt.args)
            .unwrap_or_else(|| self.nt.args.clone());
        if !args_label.is_empty() {
            content.push(Span::styled(
                format!(" {args_label}"),
                Style::new().fg(t.subtle),
            ));
        }
        let header_suffix = self
            .nt
            .preview
            .as_ref()
            .and_then(|preview| preview.header_suffix.as_deref())
            .map(str::to_owned)
            .or_else(|| native_header_suffix(&self.nt.name, self.nt.result.as_deref()));
        if let Some(note) = header_suffix {
            content.push(Span::styled(format!(" {note}"), Style::new().fg(t.subtle)));
        }
        if self.nt.done {
            content.push(Span::styled(
                format!("  {}", detail_marker(expanded)),
                Style::new().fg(t.primary),
            ));
        }
        let header_deco = vec![
            Span::raw("  "),
            Span::styled(
                if self.is_last { "└ " } else { "├ " },
                Style::new().fg(t.subtle),
            ),
            prim::status_icon(t, working, self.nt.is_error, cx.spinner()),
        ];
        let name_w = 5 + self.nt.name.chars().count();
        let cont_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::raw(" ".repeat(name_w)),
        ];
        let mut rendered = prim::rline_wrapped(header_deco, &cont_deco, content, cx.width);
        if self.nt.done {
            for line in &mut rendered {
                line.detail = Some(super::prim::DetailTarget {
                    key: key.clone(),
                    total: total.max(1),
                    tail: self.nt.name == "bash",
                });
            }
        }
        rendered.extend(detail_box(
            &lines,
            key,
            self.nt.name == "bash",
            false,
            cx,
            vec![
                Span::raw("  "),
                Span::styled(exec_cont, Style::new().fg(t.subtle)),
                Span::raw("  "),
            ],
            Style::new().fg(if self.nt.is_error { t.error } else { t.muted }),
        ));
        let mut out = RenderWindow::new(range);
        out.extend(rendered);
        out
    }
}

struct ToolLine<'a> {
    tool: &'a ToolCall,
}

impl Component for ToolLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let working = !self.tool.done && cx.active_turn;
        let icon = prim::status_icon(t, working, self.tool.is_error, cx.spinner());
        let mut content = vec![Span::styled(
            self.tool.name.clone(),
            Style::new().fg(t.info),
        )];
        if let Some(first) = self.tool.input.split('\n').next() {
            if !first.is_empty() {
                content.push(prim::subtle(format!(" {first}"), t));
            }
        }
        vec![prim::rline(vec![Span::raw("  "), icon], content)]
    }
}

struct UserShellLine<'a> {
    command: &'a str,
    output: &'a str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration: Duration,
    truncated: bool,
    cancelled: bool,
    exclude_from_context: bool,
}

impl Component for UserShellLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let failed =
            self.cancelled || self.signal.is_some() || self.exit_code.is_some_and(|c| c != 0);
        let status_color = if failed { t.error } else { t.success };
        let command_style = Style::new().fg(t.fg);
        let mut out = Vec::new();

        let command_avail = cx.width.saturating_sub(4);
        for (i, seg) in prim::wrap_pre(self.command, command_avail)
            .into_iter()
            .enumerate()
        {
            let prompt = if i == 0 {
                Span::styled("$ ", Style::new().fg(t.success))
            } else {
                Span::raw("  ")
            };
            out.push(prim::rline(
                vec![Span::raw("  "), prompt],
                vec![Span::styled(seg, command_style)],
            ));
        }

        let lines: Vec<&str> = self.output.trim_end_matches('\n').split('\n').collect();
        let limit = PREVIEW_LINES;
        let rail = vec![
            Span::raw("  "),
            Span::styled("│ ", Style::new().fg(t.subtle)),
        ];
        if !self.output.is_empty() {
            let body_style = Style::new().fg(if failed { t.error } else { t.muted });
            for raw in lines.iter().take(limit) {
                for seg in prim::wrap_pre(raw, cx.width.saturating_sub(4)) {
                    out.push(prim::rline(
                        rail.clone(),
                        vec![Span::styled(seg, body_style)],
                    ));
                }
            }
            let hidden = lines.len().saturating_sub(limit);
            if hidden > 0 {
                out.push(prim::rline(
                    rail,
                    vec![Span::styled(
                        format!("… ({hidden} lines hidden)"),
                        Style::new().fg(t.subtle),
                    )],
                ));
            }
        }

        let mut status = if self.cancelled {
            "Cancelled".to_string()
        } else if let Some(signal) = self.signal {
            format!("Signal {signal}")
        } else {
            format!("Exit {}", self.exit_code.unwrap_or(0))
        };
        let _ = write!(status, ", took {}", prim::fmt_duration(self.duration));
        if self.truncated {
            status.push_str(" · truncated");
        }
        if self.exclude_from_context {
            status.push_str(" · not in context");
        }
        out.push(prim::rline(
            vec![
                Span::raw("  "),
                Span::styled("└ ", Style::new().fg(t.subtle)),
                Span::styled(
                    if failed { "✗ " } else { "✓ " },
                    Style::new().fg(status_color),
                ),
            ],
            vec![Span::styled(status, Style::new().fg(t.fg))],
        ));
        out
    }
}

struct ErrorLine<'a> {
    msg: &'a str,
}

impl Component for ErrorLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let err = Style::new().fg(t.error);
        let content_w = cx.width.saturating_sub(4); // "  " + "✗ "
        let mut out = Vec::new();
        for (i, seg) in prim::wrap(self.msg, content_w).iter().enumerate() {
            let deco = if i == 0 {
                vec![Span::raw("  "), Span::styled("✗ ", err)]
            } else {
                vec![Span::raw("  "), Span::raw("  ")]
            };
            out.push(prim::rline(deco, vec![Span::styled(seg.clone(), err)]));
        }
        out
    }
}

/// Turn-end separator: `Done in Ns with <label>`. Appended to a turn when
/// its run finishes. Carries only model, level, and duration so the line
/// never overflows; nothing is wrapped below it.
struct TurnEnd {
    label: String,
    elapsed: Duration,
}

impl Component for TurnEnd {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        if cx.active_turn {
            return Vec::new();
        }
        let t = cx.theme;
        let dur = prim::fmt_duration(self.elapsed);
        vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.subtle)),
            ],
            vec![
                Span::styled(format!("Done in {dur} with "), Style::new().fg(t.subtle)),
                Span::styled(self.label.clone(), Style::new().fg(t.muted)),
            ],
            vec![],
        )]
    }
}

/// Turn-failed separator. Line 1 carries model, level, and duration only
/// (`◇ Failed in Ns with <label>`) so the status never overflows; the provider
/// error is wrapped below it, indented and word-broken with a wide-char
/// fallback. Mirrors [`TurnEnd`] but signals the turn did not complete;
/// the turn's partial content precedes it on the same branch.
/// When the error is empty the failure was already surfaced as a fatal `✗`
/// line (see [`ErrorLine`]) earlier in the turn, so nothing is repeated
/// below the header. The builder drops the text in that case rather than
/// rendering a redundant copy.
struct TurnFailed {
    label: String,
    elapsed: Duration,
    error: String,
}

impl Component for TurnFailed {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        if cx.active_turn {
            return Vec::new();
        }
        let t = cx.theme;
        let dur = prim::fmt_duration(self.elapsed);
        let mut out = vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.error)),
            ],
            vec![
                Span::styled(format!("Failed in {dur} with "), Style::new().fg(t.error)),
                Span::styled(self.label.clone(), Style::new().fg(t.error)),
            ],
            vec![],
        )];
        let err = self.error.trim();
        if !err.is_empty() {
            // Wrap below the header so a long provider error isn't clipped
            // at the terminal edge. Indented to align under the label;
            // blank source lines are preserved as blank wrapped lines.
            let indent = "    ";
            let content_w = cx.width.saturating_sub(indent.len());
            for raw in err.split('\n') {
                let line = raw.trim_end();
                if line.is_empty() {
                    out.push(prim::rblank());
                } else {
                    for seg in prim::wrap(line, content_w) {
                        out.push(prim::render(
                            vec![Span::raw(indent)],
                            vec![Span::styled(seg, Style::new().fg(t.error))],
                            vec![],
                        ));
                    }
                }
            }
        }
        out
    }
}

/// Turn-cancelled separator. Shows an "Operation aborted" status while
/// keeping partial assistant content immediately above it.
struct TurnCancelled {
    label: String,
    elapsed: Duration,
}

impl Component for TurnCancelled {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        if cx.active_turn {
            return Vec::new();
        }
        let t = cx.theme;
        let dur = prim::fmt_duration(self.elapsed);
        vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.error)),
            ],
            vec![
                Span::styled(
                    format!("Cancelled after {dur} with "),
                    Style::new().fg(t.error),
                ),
                Span::styled(self.label.clone(), Style::new().fg(t.error)),
            ],
            vec![],
        )]
    }
}

/// Compaction marker: `◇ Compacted N messages · kept M` in the muted tint,
/// appended to a turn when `/compact` (or the auto-trigger) folds the
/// older history into a summary. Under `/verbose` the folded summary text
/// is expanded below the marker (soft-wrapped, muted) so the fold can be
/// inspected without leaving the transcript.
struct CompactionLine {
    summarized: usize,
    kept: usize,
    summary: String,
}

impl Component for CompactionLine {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let body = format!(
            "Compacted {} messages · kept {}",
            self.summarized, self.kept
        );
        let marker = prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.subtle)),
            ],
            vec![Span::styled(body, Style::new().fg(t.muted))],
            vec![],
        );
        let mut out = vec![marker];
        if false {
            let text = self.summary.trim();
            if !text.is_empty() {
                let indent = "    ";
                let content_w = cx.width.saturating_sub(indent.len());
                for raw in text.split('\n') {
                    let line = raw.trim_end();
                    if line.is_empty() {
                        out.push(prim::rblank());
                    } else {
                        for seg in prim::wrap(line, content_w) {
                            out.push(prim::render(
                                vec![Span::raw(indent)],
                                vec![Span::styled(seg, Style::new().fg(t.muted))],
                                vec![],
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

// `active_indicator` is re-exported for the working indicator in the chrome;
// keep the import here so the component module can surface it if needed.
#[allow(unused_imports)]
use active_indicator as _;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::{
        analyze, markdown_body_height, native_body, native_preview_range, render_markdown_body,
        summarize_tool_args, trim_reasoning_summary, Align, MdBlock,
    };
    use crate::tui::theme::Theme;
    use crate::tui::NativeTool;
    use ratatui::style::Style;

    fn analyze_blocks(src: &str) -> Vec<MdBlock> {
        analyze(src, Theme::default(), Style::default())
    }
    #[test]
    fn analyze_empty_yields_no_blocks() {
        assert!(analyze_blocks("").is_empty());
    }

    #[test]
    fn list_lines_keep_their_markers() {
        let rl = render_markdown_body(
            "- Hello\n- World\n1. one\n2. two",
            Theme::default(),
            80,
            78,
            Style::default(),
            |_| vec![],
        );
        let text: Vec<String> = rl
            .iter()
            .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, ["- Hello", "- World", "1. one", "2. two"]);
    }

    #[test]
    fn analyze_paragraph_records_source_range() {
        let blocks = analyze_blocks("hello world");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            MdBlock::Paragraph { spans, src } => {
                assert_eq!(*src, 0..11);
                let text: String = spans.iter().map(|m| m.span.content.as_ref()).collect();
                assert_eq!(text, "hello world");
            }
            other => panic!(
                "expected paragraph, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    fn rendered_text(src: &str) -> Vec<String> {
        render_markdown_body(src, Theme::default(), 80, 78, Style::default(), |_| vec![])
            .iter()
            .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn html_block_tag_renders_as_text() {
        assert_eq!(rendered_text("<input>"), ["<input>"]);
    }

    #[test]
    fn inline_html_tag_renders_as_text() {
        assert_eq!(
            rendered_text("before <input> after"),
            ["before <input> after"]
        );
    }

    #[test]
    fn analyze_heading_captures_level_and_content_offset() {
        let src = "## Title here";
        let blocks = analyze_blocks(src);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            MdBlock::Heading {
                level,
                src: range,
                content_start,
            } => {
                assert_eq!(*level, 2);
                assert_eq!(*range, 0..13);
                assert_eq!(*content_start, 3); // after "## "
                                               // Content (after the `# ` prefix) slices back to the source.
                assert_eq!(&src[*content_start..range.end], "Title here");
            }
            other => panic!("expected heading, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn analyze_fenced_code_splits_body_lines_with_ranges() {
        let src = "```rust\nfn main() {}\n  more\n```";
        let blocks = analyze_blocks(src);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            MdBlock::Code { lang, lines, fence } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert_eq!(lines.len(), 2);
                assert_eq!(lines[0].0, "fn main() {}");
                assert_eq!(lines[1].0, "  more");
                // Body ranges slice back to the exact source text.
                for (line, r) in lines {
                    assert_eq!(&src[r.clone()], line);
                }
                assert_eq!(&src[fence.clone()], src);
            }
            other => panic!("expected code, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn analyze_table_captures_header_rows_and_aligns() {
        let src = "| a | b |\n|---|--:|\n| 1 | 2 |";
        let blocks = analyze_blocks(src);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            MdBlock::Table {
                header,
                aligns,
                rows,
                src: range,
            } => {
                assert_eq!(*range, 0..src.len());
                assert_eq!(header.len(), 2);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 2);
                assert!(matches!(aligns[0], Align::Left));
                assert!(matches!(aligns[1], Align::Right));
                // Cell source ranges slice back to the raw cell text.
                assert_eq!(&src[header[0].src.clone()], "a");
                assert_eq!(&src[rows[0][1].src.clone()], "2");
            }
            other => panic!("expected table, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn analyze_blockquote_splits_lines_and_prefixes() {
        let src = "> one\n> ``two``\n>\n";
        let blocks = analyze_blocks(src);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            MdBlock::Quote { lines, src: range } => {
                assert_eq!(lines.len(), 3);
                assert_eq!(lines[0].prefix_len, 2);
                assert_eq!(lines[2].prefix_len, 1); // bare ">"
                let l0: String = lines[0]
                    .spans
                    .iter()
                    .map(|m| m.span.content.as_ref())
                    .collect();
                assert_eq!(l0, "one");
                // Each line's source range slices back to the raw quote line.
                assert_eq!(&src[lines[0].src.clone()], "> one");
                assert_eq!(&src[lines[2].src.clone()], ">");
                let _ = range;
            }
            other => panic!("expected quote, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn analyze_sequences_multiple_block_kinds() {
        let src = "# H\n\npara\n\n> q\n\n```\nx\n```";
        let blocks = analyze_blocks(src);
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|b| match b {
                MdBlock::Blank { .. } => "blank",
                MdBlock::Paragraph { .. } => "para",
                MdBlock::Heading { .. } => "heading",
                MdBlock::Code { .. } => "code",
                MdBlock::Table { .. } => "table",
                MdBlock::Quote { .. } => "quote",
            })
            .collect();
        // Blank source lines between blocks are preserved as explicit blocks.
        assert_eq!(
            kinds,
            ["heading", "blank", "para", "blank", "quote", "blank", "code"]
        );
    }

    #[test]
    fn analyze_inline_offsets_are_global_source_bytes() {
        // Bold inside a paragraph after another block: span offsets must point
        // into the full source, not the block-local sub-slice.
        let src = "intro\n\nbody **bold** tail";
        let blocks = analyze_blocks(src);
        // Find the bold span across all paragraphs; its content_start must
        // locate "bold" in src.
        let bold = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Paragraph { spans, .. } => Some(spans),
                _ => None,
            })
            .flatten()
            .find(|m| m.span.content.as_ref() == "bold")
            .unwrap_or_else(|| panic!("bold span"));
        assert_eq!(&src[bold.content_start..bold.content_start + 4], "bold");
    }

    #[test]
    fn bash_preview_keeps_tail_while_other_tools_keep_head() {
        assert_eq!(native_preview_range("bash", 10, false), 7..10);
        assert_eq!(native_preview_range("read", 10, false), 0..3);
        assert_eq!(native_preview_range("bash", 2, false), 0..2);
        assert_eq!(native_preview_range("bash", 10, true), 0..10);
    }

    #[test]
    fn bash_error_body_renders_only_structured_output() {
        let raw = serde_json::json!({
            "ok": false,
            "output": "command not found\n",
            "code": 127
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "bash".to_string(),
            args: String::new(),
            result: Some(raw.clone()),
            preview: None,
            is_error: true,
            done: true,
        };
        assert_eq!(native_body(&tool).lines, vec!["command not found"]);
        assert_eq!(tool.result.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn summarize_tool_args_shortens_job_envelopes() {
        assert_eq!(
            summarize_tool_args("jobStatus", "{\"id\":\"1786294694788353138\"}").as_deref(),
            Some("job 1786294694788353138")
        );
        assert_eq!(
            summarize_tool_args("jobSpawn", "{\"cmd\":\"sleep 1\"}").as_deref(),
            Some("sleep 1")
        );
        assert_eq!(
            summarize_tool_args("jobSpawn", "{\"cmd\":\"a very long command line that exceeds the 60-character display budget\"}").as_deref(),
            Some("a very long command line that exceeds the 60-character disp…")
        );
        assert_eq!(
            summarize_tool_args("read", "{\"path\":\"/tmp/x\"}"),
            None,
            "read is not a job tool — keep verbatim"
        );
    }

    #[test]
    fn job_status_body_renders_compact_summary() {
        let raw = serde_json::json!({
            "id": "1786294694788353138",
            "state": "completed",
            "exitCode": 0,
            "signal": null,
            "durationMs": 30089,
            "logPath": "/tmp/lofi-job.log",
            "notify": true,
            "notifyChanged": true,
            "notifyIntervalMs": 5000,
            "ok": true,
            "command": "for i in 1 2; do echo $i; done",
            "directory": "/tmp",
            "timeoutMs": 30000
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "jobStatus".to_string(),
            args: String::new(),
            result: Some(raw),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert_eq!(body.lines[0], "state: completed");
        assert!(
            body.lines[1].contains("exit 0"),
            "exit code line: {:?}",
            body.lines[1]
        );
        assert!(
            body.lines[1].contains("duration"),
            "duration line: {:?}",
            body.lines[1]
        );
        let notify_line = body
            .lines
            .iter()
            .find(|l| l.starts_with("notify:"))
            .expect("notify line present");
        assert!(notify_line.contains("notify: true"), "{notify_line}");
        assert!(notify_line.contains("interval"), "{notify_line}");
        assert!(
            body.lines.last().is_some_and(|l| l.starts_with("log: ")),
            "log path last: {:?}",
            body.lines
        );
    }

    #[test]
    fn job_read_body_renders_output_with_tail_notice() {
        let raw = serde_json::json!({
            "id": "1",
            "state": "completed",
            "output": "beat-1\nbeat-2\n",
            "cursor": 14,
            "totalBytes": 71,
            "done": false,
            "ok": true
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "jobRead".to_string(),
            args: String::new(),
            result: Some(raw),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert_eq!(body.lines, vec!["beat-1", "beat-2"]);
        assert_eq!(body.notice.as_deref(), Some("(read 14 of 71 bytes)"));
    }

    #[test]
    fn job_read_body_marks_done_at_end() {
        let raw = serde_json::json!({
            "id": "1",
            "output": "done\n",
            "cursor": 71,
            "totalBytes": 71,
            "done": true,
            "ok": true
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "jobRead".to_string(),
            args: String::new(),
            result: Some(raw),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert_eq!(body.notice.as_deref(), Some("(end of log; 71 bytes total)"));
    }

    #[test]
    fn reasoning_summary_trims_empty_placeholder_parts() {
        let text = "**Checking**\n<!-- -->\n\nActual <!-- --> content.\n\n**Done**\nResult";
        assert_eq!(
            trim_reasoning_summary(text),
            "Actual <!-- --> content.\n\n**Done**\nResult"
        );
    }

    #[test]
    fn reasoning_summary_trims_plain_empty_placeholder() {
        assert_eq!(trim_reasoning_summary(" <!-- --> "), "");
    }

    #[test]
    fn heading_content_start_never_precedes_block() {
        // Bare ATX marker followed by more content: heading has no inline
        // events, so `content_start` must clamp to the block range instead of
        // the `InlineExtent` sentinel (0), which precedes `src.start`.
        for c in ["foo\n# ", "foo\n#", "#\n", "## ", "#   "] {
            for b in analyze_blocks(c) {
                if let MdBlock::Heading {
                    content_start, src, ..
                } = b
                {
                    assert!(
                        content_start >= src.start,
                        "content_start {content_start} < src.start {} for {c:?}",
                        src.start
                    );
                }
            }
        }
    }

    #[test]
    fn height_matches_render_for_bare_heading() {
        assert_height_matches("foo\n# ");
        assert_height_matches("\n\n#\n");
    }

    /// Reference height via the existing renderer: count the lines produced.
    fn reference_height(text: &str, content_w: usize) -> usize {
        render_markdown_body(
            text,
            Theme::default(),
            content_w + 2,
            content_w,
            Style::default(),
            |_| vec![],
        )
        .len()
    }

    fn assert_height_matches(text: &str) {
        for content_w in [10usize, 20, 30, 40, 80, 120] {
            let expected = reference_height(text, content_w);
            let actual = markdown_body_height(text, content_w);
            assert_eq!(
                actual, expected,
                "height mismatch at content_w={content_w} for:\n{text}",
            );
        }
    }

    #[test]
    fn height_simple_paragraph() {
        assert_height_matches("hello world");
        assert_height_matches("The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog.");
        assert_height_matches("short");
        assert_height_matches("\n".repeat(3).as_str());
    }

    #[test]
    fn height_markdown_structures() {
        assert_height_matches("# Heading one\n\nSome text under it.");
        assert_height_matches("## Sub");
        assert_height_matches("> quote\n> more quote");
        assert_height_matches(
            "> \
> not-empty",
        );
        assert_height_matches("- item one\n- item two");
        assert_height_matches("1. numbered\n2. list");
        assert_height_matches("regular\n\n```rust\nfn main() {}\n```\n\nafter");
        assert_height_matches("```\nplain\n```");
        assert_height_matches("| a | b |\n|---|---|\n| 1 | 2 |");
        assert_height_matches(
            "| very long cell content that will definitely wrap | another |\n|---|---|\n| x | y |",
        );
        assert_height_matches("**bold** and *italic* and `code` mixed inline");
        assert_height_matches("a\n\n[link](https://example.com) trail");
    }

    #[test]
    fn height_wrap_boundaries() {
        // Words exactly at the wrap boundary.
        assert_height_matches("aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee");
        // Long unbroken word forces character-level break.
        assert_height_matches("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // Mixed code and headings and wrapping.
        assert_height_matches("# A heading that is sufficiently long to wrap across multiple lines at narrow widths for sure");
        // Unicode.
        assert_height_matches("こんにちは、世界。これはテストです。もっと書きます。");
        assert_height_matches("emoji 🎉 🚀 and text together");
    }

    #[test]
    fn height_empty() {
        assert_height_matches("");
    }

    /// Seeded fuzz over many random markdown shapes: every line picked from a
    /// pool of construct patterns (headings, lists, quotes, code fences,
    /// tables, wrapping text, unicode, links) then joined. `markdown_body_height`
    /// must agree with `render_markdown_body(...).len()` for all of them, for
    /// each probe width. Both walk the same `analyze()` tree; this guards that
    /// the render and height paths never drift apart (the historical source of
    /// transcript popping on scroll/resize).
    #[test]
    fn height_fuzz_seeded() {
        // Simple deterministic PRNG (xorshift64) — no external deps.
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.0 = x;
                x
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() % n.max(1) as u64) as usize
            }
        }

        let lines_pool = [
            "plain short text",
            "The quick brown fox jumps over the lazy dog. And then some more to wrap.",
            "# h1",
            "## h2 with a rather long heading to force wrapping at narrow widths",
            "### h3",
            "#### h4",
            "##### h5",
            "###### h6",
            "- bullet item",
            "- bullet item with wrapping text to check wrapping behavior at small widths",
            "1. numbered one",
            "2. numbered two",
            "> quote single line",
            "> quote with enough content to wrap across multiple visual lines",
            "> ",
            "```rust",
            "let x = 42; // code",
            "```",
            "| col1 | col2 | col3 |",
            "|------|------|------|",
            "| a    | b    | c    |",
            "| long-cell-content-that-wraps | another | c |",
            "**bold** inline and *italic* too",
            "a `code span` in line",
            "[link text](https://example.com/some/path) and trailing",
            "unicode: こんにちは世界 🎉🚀 テスト",
            "mixed **bold** with `code` and [link](https://example.com)",
            "",    // blank line
            "   ", // whitespace-only
            ">> nested quote marker doesn\u{2019}t exist as a concept but is fine as text",
            "text with *unclosed star and **unclosed bold",
            "#   ", // bare ATX marker (empty heading)
            "# ",
            "## ",
            concat!("#", "\n"),
            "",
        ];

        for seed in 0..64u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let n_lines = 1 + rng.below(24);
            let mut text_parts: Vec<&str> = Vec::new();
            for _ in 0..n_lines {
                text_parts.push(lines_pool[rng.below(lines_pool.len())]);
            }
            let text = text_parts.join("\n");

            for content_w in [5usize, 8, 12, 16, 24, 40, 64, 96, 160] {
                let expected = reference_height(&text, content_w);
                let actual = markdown_body_height(&text, content_w);
                assert_eq!(
                    actual, expected,
                    "height mismatch (seed={seed} content_w={content_w}) for:\n{text}",
                );
            }
        }
    }
}

[Showing lines 2411-3872 of 3872 (50.0KB limit). Full output: /home/sirn/.local/state/lofi/tmp/lofi-8db8ce42-244f-52c7-a6a7-7ba09993bed3/8ae984c568b34056b22f9dcdbc0178f5/lofi-bash-18cea0463abb4518.log. Use lofi.read("/home/sirn/.local/state/lofi/tmp/lofi-8db8ce42-244f-52c7-a6a7-7ba09993bed3/8ae984c568b34056b22f9dcdbc0178f5/lofi-bash-18cea0463abb4518.log") to page through.]