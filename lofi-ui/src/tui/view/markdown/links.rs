use super::inline::MappedSpan;
use crate::tui::view::prim::Hyperlink;
use std::sync::Arc;

pub(super) fn is_safe_link_url(url: &str) -> bool {
    !url.is_empty() && !url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}

pub(super) fn mapped_hyperlinks(spans: &[MappedSpan]) -> Vec<Hyperlink> {
    let mut out: Vec<Hyperlink> = Vec::new();
    let mut pos = 0usize;
    for mapped in spans {
        let len = mapped.span.content.chars().count();
        if let Some(url) = &mapped.link {
            if let Some(previous) = out
                .last_mut()
                .filter(|link| link.end == pos && Arc::ptr_eq(&link.url, url))
            {
                previous.end += len;
            } else {
                out.push(Hyperlink {
                    start: pos,
                    end: pos + len,
                    url: url.clone(),
                });
            }
        }
        pos += len;
    }
    out
}

pub(super) fn split_links_by_rows(
    links: &[Hyperlink],
    rows: &[ratatui::text::Line<'static>],
) -> Vec<Vec<Hyperlink>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut row_start = 0usize;
    for row in rows {
        let row_len: usize = row
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        let row_end = row_start + row_len;
        out.push(
            links
                .iter()
                .filter_map(|link| {
                    let start = link.start.max(row_start);
                    let end = link.end.min(row_end);
                    (start < end).then(|| Hyperlink {
                        start: start - row_start,
                        end: end - row_start,
                        url: link.url.clone(),
                    })
                })
                .collect(),
        );
        row_start = row_end;
    }
    out
}
