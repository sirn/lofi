//! Shared SSE byte-stream decoder used by all three providers.
//!
//! The providers layer turns an HTTP response body into a stream of
//! [`SseEvent`]s here, then maps each event to a [`StreamingEvent`] via a
//! provider-specific closure. Keeping the incremental line/UTF-8 buffering in
//! one place avoids duplicating the fiddly partial-chunk logic across the
//! transports.
//!
//! Decoding handles two streaming hazards: a multibyte UTF-8 sequence may be
//! split across `bytes::Bytes` chunks, and an SSE block (terminated by a blank
//! line) may be split across chunks. A small byte buffer holds the unfinished
//! tail of each until the next chunk completes it.

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use lofi_types::StreamingEvent;

use crate::error::{Error, Result};
use crate::ir::codec::{is_done_marker, parse_sse_lines, SseEvent};

/// A boxed, owned stream of streaming events.
pub(crate) type EventStream = std::pin::Pin<Box<dyn Stream<Item = Result<StreamingEvent>> + Send>>;

/// Mapper from a parsed SSE event to zero or more [`StreamingEvent`]s.
///
/// Each provider supplies its own mapper: the `OpenAI` transports parse the
/// `data:` JSON and call the relevant `ir` mapper; Anthropic forwards the
/// `(event, data)` pair. Returning an empty vector skips keep-alive and
/// no-op blocks. Returning [`Err`] surfaces a provider-reported error
/// (e.g. an `error`/`failed`/`incomplete` event) and terminates the stream.
pub(crate) trait SseMapper: Send + 'static {
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>>;

    /// Called when the upstream byte stream ends. The default accepts EOF;
    /// providers whose protocol requires an explicit terminal event override
    /// this to return [`Err`] when none was seen, so a mid-response disconnect
    /// surfaces as an error instead of a silent partial turn.
    fn on_eof(&mut self) -> Result<()> {
        Ok(())
    }
}

impl<F> SseMapper for F
where
    F: FnMut(SseEvent) -> Result<Vec<StreamingEvent>> + Send + 'static,
{
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
        (self)(event)
    }
}

/// Unfold state holding the upstream byte stream plus partial decodings.
struct SseState<M> {
    bytes: std::pin::Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    /// Undecoded bytes waiting for the rest of a split multibyte sequence.
    pending_bytes: Vec<u8>,
    /// Decoded text waiting for the blank line that closes an SSE block.
    pending_lines: String,
    /// Parsed events not yet emitted (a single chunk can carry several).
    queued: std::collections::VecDeque<Result<StreamingEvent>>,
    /// `true` once the upstream byte stream has ended.
    exhausted: bool,
    /// `true` once a `data: [DONE]` sentinel was seen.
    done: bool,
    mapper: M,
}

/// Turn a streaming HTTP response into a stream of [`StreamingEvent`]s.
///
/// `resp.error_for_status()` should be called by the provider before invoking
/// this helper so transport-level failures surface as [`Error::Http`]. Each
/// parsed SSE block is handed to `mapper`; `data: [DONE]` terminates the
/// stream. Malformed JSON inside a `data:` line is the mapper's responsibility
/// — it returns `None` for unrecognized payloads.
pub(crate) fn map_sse_response<M: SseMapper>(resp: reqwest::Response, mapper: M) -> EventStream {
    let bytes = Box::pin(resp.bytes_stream());
    let state = SseState {
        bytes,
        pending_bytes: Vec::new(),
        pending_lines: String::new(),
        queued: std::collections::VecDeque::new(),
        exhausted: false,
        done: false,
        mapper,
    };
    futures::stream::unfold(state, step).boxed()
}

/// One unfold step: emit the next queued event, or pull and decode chunks
/// until one produces an event (or the upstream ends).
async fn step<M: SseMapper>(
    mut state: SseState<M>,
) -> Option<(Result<StreamingEvent>, SseState<M>)> {
    loop {
        if let Some(ev) = state.queued.pop_front() {
            return Some((ev, state));
        }
        if state.exhausted || state.done {
            return None;
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => feed_chunk(&mut state, &chunk),
            Some(Err(e)) => return Some((Err(Error::Http(e)), state)),
            None => {
                state.exhausted = true;
                flush_tail(&mut state);
                if !state.done {
                    // Require a provider terminal event; a disconnect before
                    // one is an error, not a successful partial turn.
                    if let Err(e) = state.mapper.on_eof() {
                        state.queued.push_back(Err(e));
                        state.done = true;
                    }
                }
            }
        }
    }
}

/// Append a chunk to the decoder, decoding complete UTF-8 and splitting out
/// any closed SSE blocks into `queued`.
fn feed_chunk<M: SseMapper>(state: &mut SseState<M>, chunk: &Bytes) {
    state.pending_bytes.extend_from_slice(chunk);
    let pending = state.pending_bytes.split_off(0);
    let valid_len = match std::str::from_utf8(&pending) {
        Ok(_) => pending.len(),
        Err(e) => e.valid_up_to(),
    };
    let decoded = std::str::from_utf8(&pending[..valid_len]).unwrap_or_default();
    state.pending_bytes.extend_from_slice(&pending[valid_len..]);
    append_text(state, decoded);
}

/// On upstream end, decode any trailing bytes and flush the final block even
/// without a closing blank line.
fn flush_tail<M: SseMapper>(state: &mut SseState<M>) {
    if state.done {
        return;
    }
    if state.pending_bytes.is_empty() {
        flush_pending_lines(state, true);
        return;
    }
    let pending = std::mem::take(&mut state.pending_bytes);
    let lossy = String::from_utf8_lossy(&pending).into_owned();
    append_text(state, &lossy);
    flush_pending_lines(state, true);
}

/// Append decoded text, splitting out closed blocks as they appear.
fn append_text<M: SseMapper>(state: &mut SseState<M>, text: &str) {
    // SSE lines may be terminated by `\r\n`, `\n`, or a lone `\r`. Normalize
    // all of them to `\n` so the `\n\n` block delimiter matches regardless
    // of the server's line ending — including a `\r\n` split across two
    // chunks, where the trailing `\r` and leading `\n` arrive separately
    // (a per-chunk `\r\n`->`\n` replace would miss that boundary).
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    state.pending_lines.push_str(&normalized);
    flush_pending_lines(state, false);
}

/// Extract any complete (`\n\n`-terminated) blocks from `pending_lines` and
/// push their mapped events onto `queued`. When `final_flush` is set, the
/// remaining text is treated as one trailing block.
fn flush_pending_lines<M: SseMapper>(state: &mut SseState<M>, final_flush: bool) {
    if state.done {
        return;
    }
    while let Some(idx) = state.pending_lines.find("\n\n") {
        let block: String = state.pending_lines.drain(..idx).collect();
        // Drop the blank-line terminator.
        state.pending_lines.drain(..2);
        enqueue_block(state, &block);
        if state.done {
            return;
        }
    }
    if final_flush {
        let remaining = std::mem::take(&mut state.pending_lines);
        let trimmed = remaining.trim_end_matches('\n');
        if !trimmed.is_empty() {
            enqueue_block(state, trimmed);
        }
    }
}

/// Parse one SSE block and push its mapped event (if any) onto `queued`.
/// `data: [DONE]` empties the queue and marks the stream exhausted so the
/// unfold terminates after any already-queued events... but per the SSE spec
/// `[DONE]` is the terminal sentinel, so we drop pending events and stop.
fn enqueue_block<M: SseMapper>(state: &mut SseState<M>, block: &str) {
    for ev in parse_sse_lines(block.lines()) {
        if is_done_marker(&ev.data) {
            // `[DONE]` is the terminal sentinel: stop accepting further
            // blocks. Already-queued events are still emitted.
            state.done = true;
            return;
        }
        match state.mapper.map(ev) {
            Ok(events) => {
                for e in events {
                    state.queued.push_back(Ok(e));
                }
            }
            Err(e) => {
                // A provider-reported error is terminal: surface it and stop.
                state.queued.push_back(Err(e));
                state.done = true;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::StreamingEvent;

    /// A mapper that pulls `data:` JSON and returns a `TextDelta` of the
    /// `text` field, mirroring the shape the real OpenAI mapper consumes.
    fn text_mapper(ev: SseEvent) -> Result<Vec<StreamingEvent>> {
        let v: serde_json::Value = serde_json::from_str(&ev.data)
            .map_err(|e| Error::Provider(format!("malformed SSE data: {e}")))?;
        Ok(v.get("text")
            .and_then(serde_json::Value::as_str)
            .map(|s| vec![StreamingEvent::TextDelta(s.to_string())])
            .unwrap_or_default())
    }

    /// Drive the decoder over a sequence of byte chunks and collect events.
    async fn run_decoder(chunks: Vec<&'static [u8]>) -> Vec<Result<StreamingEvent>> {
        let chunk_iter = chunks.into_iter().map(|c| Ok(Bytes::copy_from_slice(c)));
        let byte_stream = futures::stream::iter(chunk_iter);
        let state = SseState {
            bytes: Box::pin(byte_stream),
            pending_bytes: Vec::new(),
            pending_lines: String::new(),
            queued: std::collections::VecDeque::new(),
            exhausted: false,
            done: false,
            mapper: text_mapper,
        };
        let stream = futures::stream::unfold(state, step);
        stream.collect().await
    }

    #[tokio::test]
    async fn single_chunk_single_block() {
        let body = b"data: {\"text\":\"hi\"}\n\n";
        let out = run_decoder(vec![body.as_slice()]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("hi".to_string())
        );
    }

    #[tokio::test]
    async fn block_split_across_chunks() {
        let out = run_decoder(vec![b"data: {\"text\":\"hel", b"lo\"}\n\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("hello".to_string())
        );
    }

    #[tokio::test]
    async fn split_utf8_multibyte_across_chunks() {
        // "é" is 0xC3 0xA9 in UTF-8; split it between chunks.
        let out = run_decoder(vec![b"data: {\"text\":\"a\xC3", b"\xA9\"}\n\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("aé".to_string())
        );
    }

    #[tokio::test]
    async fn multiple_blocks_in_one_chunk() {
        let body = b"data: {\"text\":\"a\"}\n\ndata: {\"text\":\"b\"}\n\n";
        let out = run_decoder(vec![body.as_slice()]).await;
        assert_eq!(out.len(), 2);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("a".to_string())
        );
        assert_eq!(
            *out[1].as_ref().unwrap(),
            StreamingEvent::TextDelta("b".to_string())
        );
    }

    #[tokio::test]
    async fn crlf_delimiter_split_across_chunks() {
        // `\r\n\r\n` block terminator split at every byte boundary must
        // still be recognized as a blank line.
        let out = run_decoder(vec![b"data: {\"text\":\"x\"}\r", b"\n\r\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("x".to_string())
        );
    }

    #[tokio::test]
    async fn lone_cr_line_terminator() {
        // A lone `\r` is a valid SSE line terminator.
        let out = run_decoder(vec![b"data: {\"text\":\"y\"}\r\r"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("y".to_string())
        );
    }

    #[tokio::test]
    async fn done_marker_terminates_stream() {
        let body = b"data: {\"text\":\"a\"}\n\ndata: [DONE]\n\ndata: {\"text\":\"after\"}\n\n";
        let out = run_decoder(vec![body.as_slice()]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("a".to_string())
        );
    }

    #[tokio::test]
    async fn trailing_block_without_blank_line_is_flushed() {
        let out = run_decoder(vec![b"data: {\"text\":\"tail\"}"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("tail".to_string())
        );
    }

    #[tokio::test]
    async fn keeps_event_field_for_mapper() {
        let body = b"event: delta\ndata: {\"text\":\"x\"}\n\n";
        // text_mapper ignores the event field, but this confirms the block
        // parses cleanly with an event line present.
        let out = run_decoder(vec![body.as_slice()]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("x".to_string())
        );
    }
}
