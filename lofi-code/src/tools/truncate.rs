pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// The line/byte caps a tool applies to its visible output. File reads and
/// bash output share one cap by design.
/// Defaults match [`DEFAULT_MAX_LINES`] / [`DEFAULT_MAX_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncatedCap {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for TruncatedCap {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    pub content: String,
    pub truncated: bool,
    pub total_lines: usize,
    pub output_lines: usize,
    pub total_bytes: usize,
    pub output_bytes: usize,
}

#[must_use]
pub fn truncate_head(content: &str) -> Truncated {
    truncate_head_with(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
}

#[must_use]
pub fn truncate_head_with(content: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncated {
            content: content.to_string(),
            truncated: false,
            total_lines,
            output_lines: total_lines,
            total_bytes,
            output_bytes: total_bytes,
        };
    }

    let mut out: Vec<&str> = Vec::with_capacity(max_lines.min(total_lines));
    let mut out_bytes = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(i > 0);
        if out_bytes + line_bytes > max_bytes {
            break;
        }
        out.push(line);
        out_bytes += line_bytes;
    }
    let content_out = out.join("\n");
    let output_bytes = content_out.len();
    Truncated {
        content: content_out,
        truncated: true,
        total_lines,
        output_lines: out.len(),
        total_bytes,
        output_bytes,
    }
}

#[must_use]
pub fn truncate_tail(content: &str) -> Truncated {
    truncate_tail_with(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
}

#[must_use]
pub fn truncate_tail_with(content: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncated {
            content: content.to_string(),
            truncated: false,
            total_lines,
            output_lines: total_lines,
            total_bytes,
            output_bytes: total_bytes,
        };
    }

    let mut out: Vec<&str> = Vec::with_capacity(max_lines.min(total_lines));
    let mut out_bytes = 0usize;
    for line in lines.iter().rev() {
        if out.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!out.is_empty());
        if out_bytes + line_bytes > max_bytes {
            break;
        }
        out.push(line);
        out_bytes += line_bytes;
    }
    out.reverse();
    let content_out = out.join("\n");
    let output_bytes = content_out.len();
    Truncated {
        content: content_out,
        truncated: true,
        total_lines,
        output_lines: out.len(),
        total_bytes,
        output_bytes,
    }
}

#[must_use]
pub fn truncate_line(line: &str) -> String {
    truncate_line_with(line, GREP_MAX_LINE_LENGTH)
}

#[must_use]
pub fn truncate_line_with(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let kept: String = line.chars().take(max_chars).collect();
    format!("{kept}...")
}

#[must_use]
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_short_unchanged() {
        let t = truncate_head("a\nb\nc");
        assert!(!t.truncated);
        assert_eq!(t.content, "a\nb\nc");
        assert_eq!(t.output_lines, 3);
    }

    #[test]
    fn head_truncates_by_lines() {
        let big = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_head_with(&big, 10, 1_000_000);
        assert!(t.truncated);
        assert_eq!(t.output_lines, 10);
        assert_eq!(t.total_lines, 100);
        assert!(t.content.starts_with("line0\nline1"));
    }

    #[test]
    fn head_truncates_by_bytes() {
        let big = (0..50)
            .map(|_| "x".repeat(100))
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_head_with(&big, 1_000_000, 250);
        assert!(t.truncated);
        assert!(t.output_bytes <= 250);
        assert!(t.output_lines < 50);
    }

    #[test]
    fn tail_keeps_last_lines() {
        let big = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_tail_with(&big, 10, 1_000_000);
        assert!(t.truncated);
        assert_eq!(t.output_lines, 10);
        assert!(t.content.contains("line99"));
        assert!(t.content.contains("line90"));
        assert!(!t.content.contains("line89"));
    }

    #[test]
    fn line_truncation_appends_ellipsis() {
        assert_eq!(truncate_line_with("short", 10), "short");
        let long = "x".repeat(20);
        let t = truncate_line_with(&long, 10);
        assert!(t.ends_with("..."));
        assert_eq!(t.chars().count(), 13); // 10 + "..."
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }
}
