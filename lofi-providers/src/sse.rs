use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use lofi_types::StreamingEvent;

use crate::ir::ProtocolIr;
use lofi_error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[must_use]
fn parse_sse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<SseEvent> {
    let mut out = Vec::new();
    let mut event = None;
    let mut data_lines = Vec::new();
    let mut have_data = false;
    for line in lines {
        if line.is_empty() {
            if have_data {
                out.push(SseEvent {
                    event: event.take(),
                    data: data_lines.join("\n"),
                });
                data_lines.clear();
                have_data = false;
            } else {
                event = None;
            }
        } else if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            have_data = true;
        }
    }
    if have_data {
        out.push(SseEvent {
            event,
            data: data_lines.join("\n"),
        });
    }
    out
}

fn is_done_marker(data: &str) -> bool {
    data.trim() == "[DONE]"
}

pub(crate) struct IrSseMapper<I: ProtocolIr> {
    state: I::State,
}

impl<I: ProtocolIr> Default for IrSseMapper<I> {
    fn default() -> Self {
        Self {
            state: I::State::default(),
        }
    }
}

impl<I: ProtocolIr> SseMapper for IrSseMapper<I> {
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
        let data: serde_json::Value = serde_json::from_str(&event.data)
            .map_err(|error| Error::Provider(format!("malformed SSE data: {error}")))?;
        I::map_event(event.event.as_deref(), &data, &mut self.state)
    }

    fn on_eof(&mut self) -> Result<()> {
        I::on_eof(&self.state)
    }

    fn handles_done_marker(&self) -> bool {
        I::handles_done_marker()
    }

    fn defer_done_until_transport_end(&self) -> bool {
        I::defer_done_until_transport_end()
    }
}

pub(crate) type EventStream = std::pin::Pin<Box<dyn Stream<Item = Result<StreamingEvent>> + Send>>;

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

    /// Whether a mapped logical [`StreamingEvent::Done`] must be withheld
    /// until the transport's `[DONE]` marker or an accepted EOF is consumed.
    /// OpenAI-compatible servers may account a response as cancelled when the
    /// client drops the body immediately after `response.completed` or the
    /// final usage chunk, before reading the SSE sentinel.
    fn defer_done_until_transport_end(&self) -> bool {
        false
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

fn sse_error<M: SseMapper>(state: &mut SseState<M>, msg: &str) {
    state
        .queued
        .push_back(Err(Error::Provider(msg.to_string())));
    state.done = true;
}

/// Unfold state holding the upstream byte stream plus partial decodings.
struct SseState<M> {
    bytes: std::pin::Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    pending_bytes: Vec<u8>,
    pending_lines: String,
    /// A trailing `\r` carried across a chunk boundary so a split `\r\n`
    /// line ending is not mistaken for a `\n\n` block terminator.
    pending_cr: bool,
    queued: std::collections::VecDeque<Result<StreamingEvent>>,
    /// Logical completion held until an `OpenAI` transport sentinel or accepted
    /// EOF has been consumed, so dropping the returned stream cannot cancel a
    /// server response that already reported semantic completion.
    pending_done: Option<StreamingEvent>,
    /// `true` once the upstream byte stream has ended.
    exhausted: bool,
    done: bool,
    mapper: M,
}

pub(crate) fn map_sse_response<M: SseMapper>(resp: reqwest::Response, mapper: M) -> EventStream {
    let bytes = Box::pin(resp.bytes_stream());
    let state = SseState {
        bytes,
        pending_bytes: Vec::new(),
        pending_lines: String::new(),
        pending_cr: false,
        queued: std::collections::VecDeque::new(),
        pending_done: None,
        exhausted: false,
        done: false,
        mapper,
    };
    futures::stream::unfold(state, step).boxed()
}

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
                    match state.mapper.on_eof() {
                        Ok(()) => {
                            if let Some(done) = state.pending_done.take() {
                                state.queued.push_back(Ok(done));
                            }
                        }
                        Err(e) => {
                            state.pending_done = None;
                            state.queued.push_back(Err(e));
                            state.done = true;
                        }
                    }
                }
            }
        }
    }
}

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
            if let Some(done) = state.pending_done.take() {
                state.queued.push_back(Ok(done));
            }
            state.done = true;
            return;
        }
        match state.mapper.map(ev) {
            Ok(events) => {
                let defer_done = state.mapper.defer_done_until_transport_end();
                let mut terminal = false;
                for event in events {
                    if matches!(event, StreamingEvent::Done(_)) {
                        if defer_done {
                            state.pending_done = Some(event);
                        } else {
                            state.queued.push_back(Ok(event));
                            terminal = true;
                        }
                    } else {
                        state.queued.push_back(Ok(event));
                    }
                }
                if terminal {
                    state.done = true;
                    return;
                }
            }
            Err(e) => {
                state.pending_done = None;
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

    #[allow(clippy::needless_pass_by_value)]
    fn text_mapper(ev: SseEvent) -> Result<Vec<StreamingEvent>> {
        let v: serde_json::Value = serde_json::from_str(&ev.data)
            .map_err(|e| Error::Provider(format!("malformed SSE data: {e}")))?;
        Ok(v.get("text")
            .and_then(serde_json::Value::as_str)
            .map(|s| vec![StreamingEvent::TextDelta(s.to_string())])
            .unwrap_or_default())
    }

    async fn run_decoder(chunks: Vec<&'static [u8]>) -> Vec<Result<StreamingEvent>> {
        let chunk_iter = chunks.into_iter().map(|c| Ok(Bytes::copy_from_slice(c)));
        let byte_stream = futures::stream::iter(chunk_iter);
        let state = SseState {
            bytes: Box::pin(byte_stream),
            pending_bytes: Vec::new(),
            pending_lines: String::new(),
            pending_cr: false,
            queued: std::collections::VecDeque::new(),
            pending_done: None,
            exhausted: false,
            done: false,
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
            pending_done: None,
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
        let out = run_decoder(vec![b"data: {\"text\":\"x\"}\r", b"\n\r\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("x".to_string())
        );
    }

    #[tokio::test]
    async fn lone_cr_line_terminator() {
        let out = run_decoder(vec![b"data: {\"text\":\"y\"}\r\r"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("y".to_string())
        );
    }

    #[tokio::test]
    async fn crlf_line_ending_split_before_data_line() {
        let out = run_decoder(vec![b"event: foo\r", b"\ndata: {\"text\":\"z\"}\n\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("z".to_string())
        );
    }

    #[tokio::test]
    async fn invalid_utf8_byte_rejected_immediately() {
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

    #[derive(Default)]
    struct DoneMapper;

    impl SseMapper for DoneMapper {
        fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
            if event.data == "done" {
                Ok(vec![StreamingEvent::Done(lofi_types::Usage::default())])
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[derive(Default)]
    struct DeferredDoneMapper;

    impl SseMapper for DeferredDoneMapper {
        fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
            if event.data == "done" {
                Ok(vec![StreamingEvent::Done(lofi_types::Usage::default())])
            } else {
                Ok(Vec::new())
            }
        }

        fn defer_done_until_transport_end(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn deferred_done_consumes_transport_sentinel_before_emitting() {
        let sentinel_polled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = sentinel_polled.clone();
        let byte_stream = futures::stream::iter([
            Ok(Bytes::from_static(b"data: done\n\n")),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ])
        .inspect(move |chunk| {
            if chunk
                .as_ref()
                .is_ok_and(|bytes| bytes.as_ref().windows(6).any(|w| w == b"[DONE]"))
            {
                observed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let state = SseState {
            bytes: Box::pin(byte_stream),
            pending_bytes: Vec::new(),
            pending_lines: String::new(),
            pending_cr: false,
            queued: std::collections::VecDeque::new(),
            pending_done: None,
            exhausted: false,
            done: false,
            mapper: DeferredDoneMapper,
        };
        let events = futures::stream::unfold(state, step)
            .collect::<Vec<_>>()
            .await;

        assert!(sentinel_polled.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(StreamingEvent::Done(_))));
    }

    #[tokio::test]
    async fn deferred_done_is_emitted_after_accepted_eof() {
        let byte_stream =
            futures::stream::once(async { Ok(Bytes::from_static(b"data: done\n\n")) });
        let state = SseState {
            bytes: Box::pin(byte_stream),
            pending_bytes: Vec::new(),
            pending_lines: String::new(),
            pending_cr: false,
            queued: std::collections::VecDeque::new(),
            pending_done: None,
            exhausted: false,
            done: false,
            mapper: DeferredDoneMapper,
        };
        let events = futures::stream::unfold(state, step)
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(StreamingEvent::Done(_))));
    }

    #[tokio::test]
    async fn mapper_done_terminates_without_transport_eof() {
        let byte_stream =
            futures::stream::once(async { Ok(Bytes::from_static(b"data: done\n\n")) })
                .chain(futures::stream::pending());
        let state = SseState {
            bytes: Box::pin(byte_stream),
            pending_bytes: Vec::new(),
            pending_lines: String::new(),
            pending_cr: false,
            queued: std::collections::VecDeque::new(),
            pending_done: None,
            exhausted: false,
            done: false,
            mapper: DoneMapper,
        };
        let events = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            futures::stream::unfold(state, step).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(StreamingEvent::Done(_))));
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
    async fn trailing_chunks_after_done_are_not_polled_or_emitted() {
        // A terminal sentinel ends the logical response immediately. Trailing
        // chunks are not emitted, and a peer that stays open cannot hold the
        // caller hostage after completion.
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
        assert!(out[0].is_ok(), "terminal handling must remain successful");
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
        let out = run_decoder(vec![body.as_slice()]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("x".to_string())
        );
    }

    #[tokio::test]
    async fn event_only_block_is_skipped_not_errored() {
        // A block carrying only an `event:` line (no `data:`) must be skipped
        // rather than dispatched with empty data, which would make the mapper
        // fail JSON decoding and abort the stream.
        let body = b"event: ping\n\ndata: {\"text\":\"ok\"}\n\n";
        let out = run_decoder(vec![body.as_slice()]).await;
        assert!(out.iter().all(Result::is_ok));
        assert_eq!(out.len(), 1);
        assert_eq!(
            *out[0].as_ref().unwrap(),
            StreamingEvent::TextDelta("ok".to_string())
        );
    }
}
