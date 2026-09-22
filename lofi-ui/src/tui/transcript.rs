#![allow(clippy::wildcard_imports)]

use super::*;

/// Retains compressed, width-independent display models so scrolling back
/// into a resumed transcript does not rescan its JSONL file. Successful
/// native results are reduced to their collapsed previews before insertion;
/// per-turn compression avoids retaining the full Rust object graph. Entries
/// are evicted least-recently-used past a hard budget, viewport-driven
/// materializations being the only inserts — background height re-measure
/// parses without caching, so the cache stays proportional to what the user
/// has actually looked at, not to the whole transcript.
pub(super) struct CollapsedTurnCache {
    pub(super) map: HashMap<usize, Box<[u8]>>,
    /// LRU order: front is the least recently used turn index.
    pub(super) order: VecDeque<usize>,
    pub(super) retained_bytes: usize,
    pub(super) frame_hits: usize,
    pub(super) frame_misses: usize,
    pub(super) frame_materialize_us: u128,
}

impl CollapsedTurnCache {
    const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
    /// A single entry above this size is never retained; it reparses from the
    /// transcript on demand instead of flushing everyone else.
    const MAX_ENTRY_BYTES: usize = 1024 * 1024;

    pub(super) fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            retained_bytes: 0,
            frame_hits: 0,
            frame_misses: 0,
            frame_materialize_us: 0,
        }
    }

    pub(super) fn get(&mut self, idx: usize) -> Option<Arc<Turn>> {
        let turn = self.map.get(&idx).and_then(|data| {
            lz4_flex::decompress_size_prepended(data)
                .ok()
                .and_then(|json| serde_json::from_slice(&json).ok())
                .map(|mut turn| {
                    assign_detail_ids(&mut turn, idx);
                    Arc::new(turn)
                })
        });
        if turn.is_some() {
            self.frame_hits += 1;
            if let Some(pos) = self.order.iter().position(|entry| *entry == idx) {
                self.order.remove(pos);
            }
            self.order.push_back(idx);
        } else {
            self.frame_misses += 1;
        }
        turn
    }

    pub(super) fn finish_materialize(&mut self, elapsed_us: u128) {
        self.frame_materialize_us += elapsed_us;
    }

    pub(super) fn reset_frame_profile(&mut self) {
        self.frame_hits = 0;
        self.frame_misses = 0;
        self.frame_materialize_us = 0;
    }

    pub(super) fn insert(&mut self, idx: usize, turn: &Turn) {
        if self.map.contains_key(&idx) {
            return;
        }
        let Ok(json) = serde_json::to_vec(turn) else {
            return;
        };
        let data = lz4_flex::compress_prepend_size(&json).into_boxed_slice();
        let bytes = data.len();
        if bytes > Self::MAX_ENTRY_BYTES {
            return;
        }
        self.retained_bytes += bytes;
        self.order.push_back(idx);
        self.map.insert(idx, data);
        while self.retained_bytes > Self::MAX_RETAINED_BYTES {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(data) = self.map.remove(&victim) {
                self.retained_bytes = self.retained_bytes.saturating_sub(data.len());
            }
        }
    }

    pub(super) fn remove(&mut self, idx: usize) {
        if let Some(data) = self.map.remove(&idx) {
            self.retained_bytes = self.retained_bytes.saturating_sub(data.len());
            if let Some(pos) = self.order.iter().position(|entry| *entry == idx) {
                self.order.remove(pos);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.retained_bytes = 0;
        self.reset_frame_profile();
    }
}

/// Retains only viewport-adjacent rendering to avoid holding the styled
/// representation of the full session.
pub(super) struct FrozenCache {
    pub(super) order: VecDeque<usize>,
    pub(super) map: HashMap<usize, Vec<view::RenderLine>>,
}

impl FrozenCache {
    pub(super) fn new() -> Self {
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    pub(super) fn contains(&self, idx: usize) -> bool {
        self.map.contains_key(&idx)
    }

    pub(super) fn get(&self, idx: usize) -> Option<&Vec<view::RenderLine>> {
        self.map.get(&idx)
    }

    pub(super) fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }

    pub(super) fn insert(&mut self, idx: usize, entry: Vec<view::RenderLine>) {
        if self.map.insert(idx, entry).is_none() {
            self.order.push_back(idx);
        }
    }

    pub(super) fn retain_near(&mut self, visible: Option<(usize, usize)>, frozen_turns: usize) {
        let Some((first, last)) = visible else {
            self.clear();
            return;
        };
        let first = first.saturating_sub(1);
        let last = last.saturating_add(1).min(frozen_turns.saturating_sub(1));
        self.map.retain(|idx, _| (first..=last).contains(idx));
        self.order.retain(|idx| (first..=last).contains(idx));
    }
}

pub(super) struct Transcript {
    pub(super) turns: Vec<Turn>,
    /// Per-turn byte range in the transcript file (`None` for ephemeral or
    /// not-yet-committed turns). Parallel to `turns`. A frozen turn's blocks
    /// are cleared once committed; `materialize_turn` re-parses this range
    /// from the file on demand so the UI's memory stays bounded by the
    /// viewport, not the session length.
    pub(super) turn_byte_ranges: Vec<Option<(u64, u64)>>,
    pub(super) turn_event_offsets: Vec<Option<Vec<u64>>>,
    pub(super) collapsed_turns: Box<RefCell<CollapsedTurnCache>>,
    /// First durable-transcript read failure seen while rendering. Set from
    /// interior-mutability paths that only hold `&App`; surfaced as a
    /// notification after the frame so a turn silently rendering as an
    /// empty shell gets a visible explanation instead of looking like
    /// lost transcript.
    pub(super) alert: RefCell<Option<String>>,
}

pub(super) struct TranscriptLayout {
    pub(super) frozen_render: FrozenCache,
    /// Line count per frozen turn (all of them), so the viewport can be
    /// located and `total` computed without fetching rendered lines. Synced
    /// to the file-backed prefix (which may be all turns) for the active mode.
    pub(super) frozen_heights: Vec<usize>,
    /// Parallel to `frozen_heights`: true while the entry is a placeholder
    /// estimate derived from the turn byte range, still waiting for an exact
    /// re-measure by the tick loop or the viewport pass. Resumed transcripts
    /// seed estimates for every historical turn so the first frame paints
    /// without re-reading the whole session file.
    pub(super) frozen_heights_estimated: Vec<bool>,
    pub(super) render_epoch: u64,
    pub(super) frozen_epoch: u64,
    /// Viewport width the frozen cache was last built at. A resize changes
    /// the wrap width, so a mismatch discards the cache just like an epoch
    /// bump — otherwise background-padded lines keep the old (narrower)
    /// width after the terminal grows.
    pub(super) frozen_width: usize,
    /// After a width-only resize, frozen turns below this exclusive upper
    /// bound still hold heights measured at the previous width. `ensure_frozen`
    /// keeps those stale heights as the total/base source (they only differ by
    /// re-wrap, so the thumb and bottom anchor stay stable); the tick loop
    /// re-measures them back-to-front — visible bottom turns first — so a
    /// resize never stalls a frame on a long transcript. `None` once every
    /// height matches `frozen_width`.
    pub(super) height_remeasure_from: Option<usize>,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self {
            turns: Vec::new(),
            turn_byte_ranges: Vec::new(),
            turn_event_offsets: Vec::new(),
            collapsed_turns: Box::new(RefCell::new(CollapsedTurnCache::new())),
            alert: RefCell::new(None),
        }
    }
}

impl TranscriptLayout {
    pub(super) fn invalidate_turn(&mut self, turn: usize) {
        self.frozen_render.clear();
        if let Some(height) = self.frozen_heights_estimated.get_mut(turn) {
            *height = true;
            self.height_remeasure_from = Some(
                self.height_remeasure_from
                    .unwrap_or_default()
                    .max(turn.saturating_add(1)),
            );
        }
    }

    pub(super) fn new() -> Self {
        Self {
            frozen_render: FrozenCache::new(),
            frozen_heights: Vec::new(),
            frozen_heights_estimated: Vec::new(),
            render_epoch: 0,
            frozen_epoch: 0,
            frozen_width: 0,
            height_remeasure_from: None,
        }
    }
}
impl Transcript {
    pub(super) fn freeze_previous_file_backed_turn(&mut self) {
        let Some(previous) = self.turns.len().checked_sub(1) else {
            return;
        };
        if self
            .turn_byte_ranges
            .get(previous)
            .is_some_and(Option::is_some)
        {
            self.turns[previous].blocks.clear();
        }
    }

    pub(super) fn merge_last_turn_range(&mut self, byte_start: u64, byte_end: u64) {
        if let Some(range) = self.turn_byte_ranges.last_mut() {
            *range = Some(match *range {
                Some((start, end)) => (start.min(byte_start), end.max(byte_end)),
                None => (byte_start, byte_end),
            });
        }
    }

    pub(super) fn push_turn(&mut self, turn: Turn) {
        self.turns.push(turn);
        self.turn_byte_ranges.push(None);
        self.turn_event_offsets.push(None);
    }

    pub(super) fn insert_turn(&mut self, idx: usize, turn: Turn) {
        self.collapsed_turns.get_mut().clear();
        self.turns.insert(idx, turn);
        self.turn_byte_ranges.insert(idx, None);
        self.turn_event_offsets.insert(idx, None);
    }

    pub(super) fn restore_last_turn(
        &mut self,
        start: u64,
        end: u64,
        offsets: Vec<u64>,
        contiguous: bool,
    ) {
        if let Some(range) = self.turn_byte_ranges.last_mut() {
            *range = Some((start, end));
        }
        if let Some(event_offsets) = self.turn_event_offsets.last_mut() {
            if contiguous {
                *event_offsets = None;
            } else {
                *event_offsets = Some(offsets);
            }
        }
        if let Some(shell_turn) = self.turns.last_mut() {
            shell_turn.blocks.clear();
        }
    }

    pub(super) fn clear(&mut self) {
        self.turns.clear();
        self.collapsed_turns.get_mut().clear();
        self.turn_byte_ranges.clear();
        self.turn_event_offsets.clear();
    }
}

impl App {
    pub(super) fn take_transcript_alert(&mut self) -> Option<String> {
        self.transcript.alert.get_mut().take()
    }

    pub(super) fn bump_render_epoch(&mut self) {
        self.layout.render_epoch = self.layout.render_epoch.wrapping_add(1);
    }

    pub(super) fn turn_has_expanded_detail(&self, idx: usize) -> bool {
        self.expanded_details
            .values()
            .any(|state| state.turn == idx)
    }

    /// True when the turn's in-memory blocks dropped released exec results
    /// but the committed transcript range can restore them. Expanded details
    /// need that durable content.
    pub(super) fn needs_durable_materialize(&self, idx: usize) -> bool {
        self.turn_has_expanded_detail(idx)
            && self
                .transcript
                .turn_byte_ranges
                .get(idx)
                .is_some_and(Option::is_some)
    }

    /// Reconstruct a frozen turn's blocks. If the turn still holds its blocks
    /// in memory (ephemeral session, or not yet frozen), clone them. Otherwise
    /// re-parse the turn's byte range from the transcript file. On any read or
    /// parse failure the turn's prompt is preserved with empty blocks.
    pub(super) fn materialize_turn(&self, idx: usize) -> Arc<Turn> {
        self.transcript.materialize_turn_caching(
            idx,
            true,
            self.turn_has_expanded_detail(idx),
            self.session.cursor.as_ref(),
        )
    }

    /// Materialize for a throwaway purpose (height measurement) that must not
    /// claim a cache slot: background re-measure walks every resumed turn, and
    /// caching its results would retain the whole transcript.
    fn peek_transcript_turn(&self, idx: usize) -> Arc<Turn> {
        self.transcript.materialize_turn_caching(
            idx,
            false,
            self.turn_has_expanded_detail(idx),
            self.session.cursor.as_ref(),
        )
    }

    pub(super) fn ensure_frozen_turn(&mut self, idx: usize, width: usize) {
        const MAX_CACHED_TURN_ROWS: usize = 4096;
        if self.layout.frozen_render.contains(idx)
            || self
                .layout
                .frozen_heights
                .get(idx)
                .copied()
                .unwrap_or(usize::MAX)
                > MAX_CACHED_TURN_ROWS
        {
            return;
        }
        let theme = self.theme;
        let turn = self.materialize_turn(idx);
        let lines = {
            let cx = view::component::Cx {
                app: self,
                theme,
                width,
                active_turn: false,
            };
            view::blocks::render_turn_lines(&cx, &turn)
        };
        self.layout.frozen_render.insert(idx, lines);
    }

    pub(super) fn frozen_turn_window(
        &self,
        idx: usize,
        width: usize,
        range: std::ops::Range<usize>,
    ) -> Vec<view::RenderLine> {
        let theme = self.theme;
        let turn = self.materialize_turn(idx);
        let cx = view::component::Cx {
            app: self,
            theme,
            width,
            active_turn: false,
        };
        view::blocks::render_turn_window(&cx, &turn, range)
    }

    /// Sync the frozen-turn cache to the current `turns`. Normally frozen turns
    /// are all but the live mutable last one; an idle fully file-backed view can
    /// freeze the final turn too.
    /// On a wholesale replacement (`bump_render_epoch`) the cache is dropped;
    /// otherwise newly-superseded turns are rendered once, their height
    /// recorded permanently in `frozen_heights`. Their temporary rendered lines
    /// are dropped here; the viewport pass caches only nearby turns. Heights
    /// are kept for every frozen turn so the viewport can be located and the
    /// scroll total computed without holding all rendered lines in memory.
    /// A viewport resize (width change) also drops the cache, since wrapping
    /// and background padding depend on width.
    pub(super) fn ensure_frozen(&mut self, width: usize) {
        let epoch_changed = self.layout.frozen_epoch != self.layout.render_epoch;
        let width_changed = self.layout.frozen_width != width;
        if epoch_changed {
            // Content changed wholesale: every cached height is invalid, so
            // drop and refill below. No incremental path here — the turns
            // themselves are different.
            self.layout.frozen_render.clear();
            self.layout.frozen_heights.clear();
            self.layout.frozen_heights_estimated.clear();
            self.layout.frozen_epoch = self.layout.render_epoch;
            self.layout.frozen_width = width;
            self.layout.height_remeasure_from = None;
        } else if width_changed {
            // Width-only resize: the styled lines in `frozen_render` are
            // bound to the old width and must be rebuilt, but the turn
            // content is identical — only its wrap changes. Keep the stale
            // heights as the total/base source so the bottom anchor and
            // thumb stay put, and re-measure incrementally (visible window
            // now, the rest on the tick loop) instead of stalling this frame.
            self.layout.frozen_render.clear();
            self.layout.frozen_width = width;
            // Stale heights are estimates at the new width: the tick loop
            // re-measures them.
            for estimated in &mut self.layout.frozen_heights_estimated {
                *estimated = true;
            }
            // Re-measure from the end: the cursor is an exclusive upper
            // bound that remeasure_heights_step walks down to zero.
            self.layout.height_remeasure_from = Some(self.layout.frozen_heights.len());
        }
        let n = self.transcript.turns.len();
        // Normally the last turn is live (or retained in memory after resume),
        // so only its prefix is frozen. /tree rollback deliberately drops the
        // selected last turn's blocks too; when it is idle and file-backed,
        // include it in the frozen set so production rendering materializes it
        // through the bounded cache instead of treating an empty shell as live.
        let target = if !self.run_active()
            && self
                .transcript
                .turns
                .last()
                .is_some_and(|turn| turn.blocks.is_empty())
            && self
                .transcript
                .turn_byte_ranges
                .last()
                .is_some_and(Option::is_some)
        {
            n
        } else {
            n.saturating_sub(1)
        };
        // A from-scratch fill (empty heights) over file-backed turns seeds
        // byte-range estimates instead of measuring: exact heights for the
        // whole transcript mean re-reading the whole session file before the
        // first frame. Estimates are good enough for viewport math off screen;
        // the viewport pass measures visible turns exactly and the tick loop
        // converges the rest.
        let seed = self.layout.frozen_heights.is_empty();
        while self.layout.frozen_heights.len() < target {
            // A frozen turn must not materialize its complete styled
            // output; the viewport pass renders only rows it needs.
            let idx = self.layout.frozen_heights.len();
            let estimate = seed
                && self
                    .transcript
                    .turn_byte_ranges
                    .get(idx)
                    .copied()
                    .flatten()
                    .is_some();
            if estimate {
                self.layout
                    .frozen_heights
                    .push(self.estimate_turn_height(idx, width));
                self.layout.frozen_heights_estimated.push(true);
            } else {
                let height = self.measure_turn_height(idx, width);
                self.layout.frozen_heights.push(height);
                self.layout.frozen_heights_estimated.push(false);
            }
        }
        if self
            .layout
            .frozen_heights_estimated
            .iter()
            .any(|&estimated| estimated)
        {
            // Exact heights converge from the bottom: the remeasure cursor is
            // an exclusive upper bound walked down by the tick loop.
            if self.layout.height_remeasure_from.is_none() {
                self.layout.height_remeasure_from = Some(self.layout.frozen_heights.len());
            }
        }
        if self.layout.frozen_heights.len() > target {
            self.layout.frozen_heights.truncate(target);
            self.layout.frozen_heights_estimated.truncate(target);
            self.layout.frozen_render.map.retain(|idx, _| *idx < target);
            self.layout.frozen_render.order.retain(|idx| *idx < target);
            // A truncated pending re-measure must not run past the new end.
            if let Some(hi) = self.layout.height_remeasure_from {
                self.layout.height_remeasure_from =
                    if hi > target { Some(target) } else { Some(hi) };
            }
        }
    }

    /// Rough line-count guess for a file-backed turn. Used as a placeholder
    /// until `remeasure_heights_step` or the viewport pass replace it with the
    /// exact measured height, so it never has to be right — only cheap. Undershoot
    /// vs overshoot merely jitter off-screen scroll offsets while they converge.
    fn estimate_turn_height(&self, idx: usize, width: usize) -> usize {
        let Some((start, end)) = self.transcript.turn_byte_ranges.get(idx).copied().flatten()
        else {
            return 4;
        };
        let bytes = usize::try_from(end.saturating_sub(start)).unwrap_or(usize::MAX);
        bytes / (width.max(1) * 2) + 3
    }

    /// Render-height of one frozen turn at `width`. Shared by the eager fill
    /// in `ensure_frozen` and the incremental resize re-measure.
    fn measure_turn_height(&self, idx: usize, width: usize) -> usize {
        let theme = self.theme;
        let turn = self.peek_transcript_turn(idx);
        let cx = view::component::Cx {
            app: self,
            theme,
            width,
            active_turn: false,
        };
        view::blocks::render_turn_height(&cx, &turn)
    }

    /// Re-measure up to `budget` stale frozen heights at the current
    /// `frozen_width`, advancing `height_remeasure_from`. Runs on the tick
    /// loop after a width-only resize so no single frame pays the full cost.
    /// Returns true while work remains (so the caller keeps the frame dirty).
    pub(super) fn remeasure_heights_step(&mut self, budget: usize) -> bool {
        let next_hi = match self.layout.height_remeasure_from {
            Some(hi) => hi.min(self.layout.frozen_heights.len()),
            None => return false,
        };
        if next_hi == 0 {
            self.layout.height_remeasure_from = None;
            return false;
        }
        let width = self.layout.frozen_width;
        // Work back-to-front: the viewport is almost always pinned to the
        // bottom, so the last frozen turns are the visible ones. Measuring
        // those first makes the on-screen content exact on the first tick;
        // the off-screen prefix converges over the following ticks. Heights
        // already exact (measured by the viewport pass, or never estimated)
        // are skipped without spending budget.
        let mut idx = next_hi;
        let mut measured = 0;
        while idx > 0 && measured < budget {
            idx -= 1;
            if self.layout.frozen_heights_estimated[idx] {
                self.layout.frozen_heights[idx] = self.measure_turn_height(idx, width);
                self.layout.frozen_heights_estimated[idx] = false;
                measured += 1;
            }
        }
        if idx == 0 {
            self.layout.height_remeasure_from = None;
            false
        } else {
            self.layout.height_remeasure_from = Some(idx);
            true
        }
    }

    pub(super) fn sync_frozen_cache_for_viewport(
        &mut self,
        off: usize,
        height: usize,
        width: usize,
    ) {
        let frozen = self.layout.frozen_heights.len();
        if frozen == 0 {
            self.layout.frozen_render.clear();
            return;
        }
        // Stored heights drive which rows of a turn are drawn, so window and
        // margin turns need exact values even while the off-screen prefix is
        // still estimated. Measure those first, then recompute the window —
        // one correction pass settles the selection in practice.
        let mut visible = self.visible_frozen_range(off, height);
        for _ in 0..2 {
            let (first, last) = visible;
            let lo = first.saturating_sub(1);
            let hi = last.saturating_add(1).min(frozen - 1);
            let mut measured = false;
            for idx in lo..=hi {
                if self.layout.frozen_heights_estimated[idx] {
                    self.layout.frozen_heights[idx] = self.measure_turn_height(idx, width);
                    self.layout.frozen_heights_estimated[idx] = false;
                    measured = true;
                }
            }
            visible = self.visible_frozen_range(off, height);
            if !measured {
                break;
            }
        }
        self.layout.frozen_render.retain_near(Some(visible), frozen);
        let (first, last) = visible;
        let first = first.saturating_sub(1);
        let last = last.saturating_add(1).min(frozen - 1);
        for idx in first..=last {
            self.ensure_frozen_turn(idx, width);
        }
    }

    /// Turns whose stored height intersects the window
    /// [off, off + height). Falls back to the last turn when the window is
    /// past the end.
    fn visible_frozen_range(&self, off: usize, height: usize) -> (usize, usize) {
        let frozen = self.layout.frozen_heights.len();
        let end = off.saturating_add(height);
        let mut pos = 0usize;
        let mut visible: Option<(usize, usize)> = None;
        for idx in 0..frozen {
            let turn_end = pos.saturating_add(self.layout.frozen_heights[idx]);
            if turn_end > off && pos < end {
                visible = Some(visible.map_or((idx, idx), |(first, _)| (first, idx)));
            }
            pos = turn_end.saturating_add(1);
        }
        visible.unwrap_or((frozen - 1, frozen - 1))
    }
}

impl Transcript {
    /// Remember the first durable-transcript read failure seen this frame;
    /// rendering holds `&App`, so interior mutability defers the notify to
    /// the event loop.
    fn note_transcript_failure(&self, message: String) {
        let mut alert = self.alert.borrow_mut();
        if alert.is_none() {
            *alert = Some(message);
        }
    }

    fn restore_result_availability(
        turn: &mut Turn,
        available_exec_results: &std::collections::HashSet<String>,
    ) {
        for block in &mut turn.blocks {
            if let Block::Tool(tool) = block {
                if available_exec_results.contains(&tool.id) {
                    tool.result_availability = ResultAvailability::Available;
                }
            }
        }
    }

    fn materialize_turn_caching(
        &self,
        idx: usize,
        cache_insert: bool,
        expanded: bool,
        cursor: Option<&store::SessionCursor>,
    ) -> Arc<Turn> {
        if let Some(turn) = self.turns.get(idx) {
            // A committed turn may have released its exec results from
            // memory; an expanded detail still needs the durable content,
            // so reparse the turn from the transcript instead.
            let durable = expanded && self.turn_byte_ranges.get(idx).is_some_and(Option::is_some);
            if !turn.blocks.is_empty() && !durable {
                return Arc::new(turn.clone());
            }
        }
        let cacheable = !expanded;
        if cacheable {
            if let Some(turn) = self.collapsed_turns.borrow_mut().get(idx) {
                return turn;
            }
        }
        let materialize_started = Instant::now();
        let turn = self.parse_transcript_turn(idx, expanded, cursor);
        if cacheable && cache_insert {
            let mut cache = self.collapsed_turns.borrow_mut();
            cache.insert(idx, &turn);
            cache.finish_materialize(materialize_started.elapsed().as_micros());
        }
        turn
    }

    fn parse_transcript_turn(
        &self,
        idx: usize,
        expanded: bool,
        cursor: Option<&store::SessionCursor>,
    ) -> Arc<Turn> {
        let prompt = self
            .turns
            .get(idx)
            .map(|t| t.prompt.clone())
            .unwrap_or_default();
        let empty = Turn {
            prompt,
            kind: lofi_types::PromptKind::User,
            blocks: Vec::new(),
        };
        let Some(cursor) = cursor else {
            return Arc::new(empty);
        };
        let selected_offsets = self.turn_event_offsets.get(idx).and_then(Option::as_deref);
        let events = if let Some(offsets) = selected_offsets {
            cursor.display_events_at(offsets)
        } else {
            let Some((start, end)) = self.turn_byte_ranges.get(idx).copied().flatten() else {
                return Arc::new(empty);
            };
            cursor.display_events_in_range(start, end)
        };
        let mut events = match events {
            Ok(events) => events,
            Err(error) => {
                self.note_transcript_failure(format!(
                    "transcript read failed (turn {}): {error}",
                    idx + 1
                ));
                return Arc::new(empty);
            }
        };
        let mut exec_ids = std::collections::HashSet::new();
        let mut available_exec_results = std::collections::HashSet::new();
        for event in &mut events {
            // Successful exec results are not rendered at all in collapsed
            // mode (the nested native-tool rows already show the work). Track
            // exec call ids from assistant messages, then discard only their
            // matching hidden result bodies. Results of other tools and all
            // errors remain intact because their collapsed previews are
            // visible. Expanded rows reparse the exact durable content.
            if !expanded {
                if let SessionEventKind::Message(message) = &mut event.kind {
                    if message.role == Role::Assistant {
                        exec_ids.extend(message.blocks.iter().filter_map(|block| match block {
                            ContentBlock::ToolUse { id, name, .. } if name == "exec" => {
                                Some(id.clone())
                            }
                            _ => None,
                        }));
                    }
                    for block in &mut message.blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error: false,
                            ..
                        } = block
                        {
                            if exec_ids.contains(tool_use_id) {
                                if !content.is_empty() {
                                    available_exec_results.insert(tool_use_id.clone());
                                }
                                content.clear();
                                content.shrink_to_fit();
                            }
                        }
                    }
                }
            }
        }
        let turns = if selected_offsets.is_some() {
            turns_from_selected_session_events(&events)
        } else {
            turns_from_session_events(&events)
        };
        let mut turn = turns.into_iter().next().unwrap_or(empty);
        assign_detail_ids(&mut turn, idx);
        Self::restore_result_availability(&mut turn, &available_exec_results);
        if !expanded {
            view::blocks::compact_native_previews(&mut turn);
        }
        Arc::new(turn)
    }
}
