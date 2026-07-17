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

use crate::ir::codec::{is_done_marker, parse_sse_lines, SseEvent};
use lofi_error::{Error, Result};

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

    /// Whether this protocol treats `data: [DONE]` as a terminal sentinel.
    /// `OpenAI` transports do; Anthropic terminates via a `message_stop` event
    /// and must not accept a stray `[DONE]` as completion.
    fn handles_done_marker(&self) -> bool {
        true
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

/// Maximum bytes buffered for a single in-flight SSE event before the stream
/// is rejected as malformed/oversized, bounding memory for an unterminated or
/// hostile provider stream.
const MAX_SSE_PENDING_BYTES: usize = 1024 * 1024;

/// Maximum bytes to drain from the upstream after the terminal sentinel
/// (data: [DONE] or `message_stop`) before giving up and dropping the
/// connection. Some providers emit a trailing cost or
/// usage chunk *after* [DONE]; reading it to EOF lets the server close
/// the socket cleanly instead of seeing EPIPE, which it would otherwise
/// log as a client disconnect. The cap bounds a misbehaving upstream that
/// keeps sending after the sentinel without ever ending.
const MAX_DRAIN_BYTES: usize = 64 * 1024;

/// Push a fatal decode error and stop the stream.
fn sse_error<M: SseMapper>(state: &mut SseState<M>, msg: &str) {
    state
        .queued
        .push_back(Err(Error::Provider(msg.to_string())));
    state.done = true;
}

/// Unfold state holding the upstream byte stream plus partial decodings.
struct SseState<M> {
    bytes: std::pin::Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    /// Undecoded bytes waiting for the rest of a split multibyte sequence.
    pending_bytes: Vec<u8>,
    /// Decoded text waiting for the blank line that closes an SSE block.
    pending_lines: String,
    /// A trailing `\r` carried across a chunk boundary so a split `\r\n`
    /// line ending is not mistaken for a `\n\n` block terminator.
    pending_cr: bool,
    /// Parsed events not yet emitted (a single chunk can carry several).
    queued: std::collections::VecDeque<Result<StreamingEvent>>,
    /// `true` once the upstream byte stream has ended.
    exhausted: bool,
    /// `true` once a terminal sentinel (`data: [DONE]` or `message_stop`)
    /// was seen. The unfold still drains the upstream to EOF (see
    /// [`MAX_DRAIN_BYTES`]) so the server can close cleanly, then stops.
    done: bool,
    /// Bytes consumed by the post-sentinel drain, bounded by
    /// [`MAX_DRAIN_BYTES`]. Once the cap is hit the connection is dropped
    /// rather than reading indefinitely.
    drained: usize,
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
        pending_cr: false,
        queued: std::collections::VecDeque::new(),
        exhausted: false,
        done: false,
        drained: 0,
        mapper,
    };
    futures::stream::unfold(state, step).boxed()
}

/// One unfold step: emit the next queued event, or pull and decode chunks
/// until one produces an event (or the upstream ends). Once the terminal
/// sentinel is seen, the remaining upstream bytes are drained (bounded by
/// [`MAX_DRAIN_BYTES`]) before the unfold terminates, so providers that emit
/// a trailing cost/usage chunk after [DONE] see a clean EOF instead of an
/// EPIPE they would log as a client disconnect.
async fn step<M: SseMapper>(
    mut state: SseState<M>,
) -> Option<(Result<StreamingEvent>, SseState<M>)> {
    loop {
        if let Some(ev) = state.queued.pop_front() {
            return Some((ev, state));
        }
        if state.exhausted {
            return None;
        }
        if state.done {
            // Terminal sentinel seen: drain the remaining upstream so the
            // server closes the socket cleanly, then stop. Stop draining
            // once the cap is hit; a well-behaved provider ends within a
            // chunk or two, while a misbehaving one is dropped rather than
            // read indefinitely. A transport error during the drain is
            // ignored since the stream already terminated successfully.
            while state.drained < MAX_DRAIN_BYTES {
                match state.bytes.next().await {
                    Some(Ok(chunk)) => {
                        state.drained = state.drained.saturating_add(chunk.len());
                    }
                    _ => break,
                }
            }
            state.exhausted = true;
            return None;
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => feed_chunk(&mut state, &chunk),
            Some(Err(e)) => return Some((Err(Error::Http(e.to_string())), state)),
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
        Err(e) => match e.error_len() {
            // A permanently invalid byte (not a truncated multi-byte tail):
            // reject immediately instead of buffering it and every later
            // chunk until EOF.
            Some(_) => {
                sse_error(state, "invalid UTF-8 in SSE stream");
                return;
            }
            None => e.valid_up_to(),
        },
    };
    let decoded = std::str::from_utf8(&pending[..valid_len]).unwrap_or_default();
    state.pending_bytes.extend_from_slice(&pending[valid_len..]);
    append_text(state, decoded);
    // `pending_bytes` now holds only an incomplete UTF-8 tail; cap it so a
    // hostile stream of partial sequences can't grow it without bound. The
    // in-flight event tail is bounded separately in `append_text`.
    if state.pending_bytes.len() > MAX_SSE_PENDING_BYTES {
        sse_error(state, "SSE event exceeded maximum buffered size");
    }
}

/// On upstream end, decode any trailing bytes and flush the final block even
/// without a closing blank line.
fn flush_tail<M: SseMapper>(state: &mut SseState<M>) {
    if state.done {
        return;
    }
    // A deferred trailing `\r` at EOF is a lone-CR line ending.
    if state.pending_cr {
        state.pending_lines.push('\n');
        state.pending_cr = false;
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
    // SSE lines may be terminated by `\r\n`, `\n`, or a lone `\r`; all are
    // normalized to `\n` so the `\n\n` block delimiter matches regardless of
    // the server's line ending. A trailing `\r` split across a chunk boundary
    // is deferred (`pending_cr`): if the next chunk begins with `\n` it is the
    // LF of a `\r\n` pair (one line ending), not a second blank line, so we
    // consume it instead of emitting `\n\n` and prematurely closing the block.
    let mut buf = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    if state.pending_cr {
        if let Some('\n') = chars.peek() {
            chars.next();
        }
        buf.push('\n');
        state.pending_cr = false;
    }
    while let Some(c) = chars.next() {
        if c == '\r' {
            match chars.peek() {
                Some('\n') => {
                    chars.next();
                    buf.push('\n');
                }
                None => state.pending_cr = true,
                _ => buf.push('\n'),
            }
        } else {
            buf.push(c);
        }
    }
    state.pending_lines.push_str(&buf);
    // Drain complete blocks first, then bound only the unfinished tail so a
    // large chunk of many small terminated events decodes successfully.
    flush_pending_lines(state, false);
    if state.pending_lines.len() > MAX_SSE_PENDING_BYTES {
        sse_error(state, "SSE event exceeded maximum buffered size");
    }
}

/// Extract any complete (`\n\n`-terminated) blocks from `pending_lines` and
/// push their mapped events onto `queued`. When `final_flush` is set, the
/// remaining text is treated as one trailing block.
fn flush_pending_lines<M: SseMapper>(state: &mut SseState<M>, final_flush: bool) {
    if state.done {
        return;
    }
    // Process complete (`\n\n`-terminated) blocks in one pass, advancing a
    // start cursor instead of draining the front each iteration (which would
    // shift the whole tail and make a chunk of many small events quadratic).
    let mut start = 0;
    while let Some(rel) = state.pending_lines[start..].find("\n\n") {
        let abs = start + rel;
        let block: String = state.pending_lines[start..abs].to_string();
        start = abs + 2;
        enqueue_block(state, &block);
        if state.done {
            return;
        }
    }
    if start > 0 {
        // Drop the processed prefix, keeping only the unfinished tail.
        let tail = state.pending_lines.split_off(start);
        state.pending_lines = tail;
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
        if is_done_marker(&ev.data) && state.mapper.handles_done_marker() {
            // `[DONE]` is the terminal sentinel for OpenAI transports: stop
            // accepting further blocks. Already-queued events are still
            // emitted. Anthropic does not use this sentinel.
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
    /// `text` field, mirroring the shape the real `OpenAI` mapper consumes.
    #[allow(clippy::needless_pass_by_value)]
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
            pending_cr: false,
            queued: std::collections::VecDeque::new(),
            exhausted: false,
            done: false,
            drained: 0,
            mapper: text_mapper,
        };
        let stream = futures::stream::unfold(state, step);
        stream.collect().await
    }

    async fn run_decoder_owned(chunks: Vec<Vec<u8>>) -> Vec<Result<StreamingEvent>> {
        let chunk_iter = chunks.into_iter().map(|c| Ok(Bytes::from(c)));
        let byte_stream = futures::stream::iter(chunk_iter);
        let state = SseState {
            bytes: Box::pin(byte_stream),
            pending_bytes: Vec::new(),
            pending_lines: String::new(),
            pending_cr: false,
            queued: std::collections::VecDeque::new(),
            exhausted: false,
            done: false,
            drained: 0,
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
    async fn crlf_line_ending_split_before_data_line() {
        // `event: ...\r\n` split between CR and LF, with a `data:` line after.
        // The `\r\n` is a single line ending, not a `\n\n` block terminator:
        // the block must contain both the event and data lines, not two
        // separate empty-data blocks.
        let out = run_decoder(vec![b"event: foo\r", b"\ndata: {\"text\":\"z\"}\n\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("z".to_string())
        );
    }

    #[tokio::test]
    async fn invalid_utf8_byte_rejected_immediately() {
        // 0xFF is never a valid UTF-8 lead byte; the stream must fail instead
        // of buffering it (and every later chunk) until EOF.
        let out = run_decoder(vec![b"data: ", &[0xFF], b"\n\n"]).await;
        assert!(out.iter().any(Result::is_err));
    }

    #[tokio::test]
    async fn oversized_event_without_terminator_is_rejected() {
        // A block with no blank-line terminator must not grow the buffer
        // without bound; the decoder rejects once the cap is exceeded.
        let big: Vec<u8> = std::iter::repeat_n(b'x', 2 * 1024 * 1024).collect();
        let out = run_decoder_owned(vec![big]).await;
        assert!(out.iter().any(Result::is_err));
    }

    #[tokio::test]
    async fn large_chunk_of_many_small_events_decodes() {
        // A single chunk larger than the cap, but composed of many small
        // terminated events, must decode successfully; the cap applies only to
        // an unfinished in-flight event, not the whole chunk.
        let event = b"data: {\"text\":\"a\"}\n\n";
        let mut chunk = Vec::new();
        let n = (2 * 1024 * 1024) / event.len() + 1;
        for _ in 0..n {
            chunk.extend_from_slice(event);
        }
        let out = run_decoder_owned(vec![chunk]).await;
        assert!(out.iter().all(Result::is_ok));
        assert!(out.len() > 1);
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
    async fn trailing_chunks_after_done_are_drained_not_emitted() {
        // Some OpenAI-compatible providers emit a trailing
        // cost/usage chunk *after* `data: [DONE]`. The decoder must drain it
        // to EOF so the server sees a clean close instead of EPIPE, while
        // still emitting only the pre-sentinel events. Splitting the trailing
        // chunk separately exercises the post-done drain loop.
        let chunks: Vec<&'static [u8]> = vec![
            b"data: {\"text\":\"a\"}\n\ndata: [DONE]\n\n",
            b"data: {\"choices\":[],\"cost\":\"0\"}\n\n",
        ];
        let out = run_decoder(chunks).await;
        assert_eq!(
            out.len(),
            1,
            "trailing chunk must not produce events: {out:?}"
        );
        assert!(out[0].is_ok(), "drain must not surface a transport error");
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
