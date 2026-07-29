#![allow(clippy::wildcard_imports)]

use super::*;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::mem::size_of;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) struct DebugState {
    file: Option<File>,
    path: Option<PathBuf>,
    session_path: Option<PathBuf>,
    debug_dir: PathBuf,
    started: Instant,
    latest_rss_bytes: Option<u64>,
    latest_heap_bytes: Option<u64>,
    previous_sample: Option<SampleTotals>,
}

#[derive(Clone, Copy)]
struct SampleTotals {
    rss: u64,
    private_dirty: u64,
    anonymous: u64,
    allocator_allocated: Option<u64>,
    components: u64,
}

#[derive(Default)]
struct AllocatorMemory {
    allocated: Option<u64>,
    free: Option<u64>,
    arena: Option<u64>,
    mmap: Option<u64>,
    releasable: Option<u64>,
}

#[derive(Default)]
struct MappingMemory {
    rss: u64,
    pss: u64,
    private_dirty: u64,
}

#[derive(Default)]
struct MappingBreakdown {
    executable: MappingMemory,
    shared_libraries: MappingMemory,
    heap: MappingMemory,
    stacks: MappingMemory,
    anonymous: MappingMemory,
    other_files: MappingMemory,
    kernel: MappingMemory,
}

#[derive(Default)]
struct ProcessMemory {
    rss_bytes: u64,
    rss_peak_bytes: u64,
    virtual_bytes: u64,
    rss_anon_bytes: u64,
    rss_file_bytes: u64,
    swap_bytes: u64,
    pss_bytes: u64,
    pss_anon_bytes: u64,
    pss_file_bytes: u64,
    pss_shmem_bytes: u64,
    private_clean_bytes: u64,
    private_dirty_bytes: u64,
    shared_clean_bytes: u64,
    shared_dirty_bytes: u64,
    anonymous_bytes: u64,
    lazy_free_bytes: u64,
    anon_huge_pages_bytes: u64,
    threads: u64,
    cpu_user_ticks: u64,
    cpu_system_ticks: u64,
    mappings: MappingBreakdown,
}

impl App {
    pub(super) fn enable_debug_from_env(&mut self) {
        if std::env::var_os("LOFI_DEBUG").is_none() || self.debug.is_some() {
            return;
        }
        match DebugState::create() {
            Ok(debug) => {
                self.debug = Some(debug);
                self.debug_sample("enabled");
            }
            Err(error) => {
                self.notify(NotifyKind::Error, format!("enable debug logging: {error}"));
            }
        }
    }

    pub(super) fn toggle_debug(&mut self) {
        if self.debug.is_some() {
            self.debug_sample("disabled");
            if let Some(debug) = self.debug.take() {
                let path = debug.path.as_ref().map_or_else(
                    || "waiting for session log".to_string(),
                    |path| path.display().to_string(),
                );
                self.notify(NotifyKind::Info, format!("debug logging disabled · {path}"));
            }
            return;
        }
        match DebugState::create() {
            Ok(debug) => {
                self.debug = Some(debug);
                self.debug_sample("enabled");
                self.notify(NotifyKind::Info, "Debug mode activated");
            }
            Err(e) => self.notify(NotifyKind::Error, format!("enable debug logging: {e}")),
        }
    }

    pub(super) fn debug_sample(&mut self, event: &str) {
        let Some(mut debug) = self.debug.take() else {
            return;
        };
        if let Err(e) = debug.write_sample(self, event) {
            self.notify(NotifyKind::Error, format!("debug logging stopped: {e}"));
        } else {
            self.debug = Some(debug);
        }
    }

    pub(super) fn debug_render_timing(
        &mut self,
        frame_width: u16,
        frame_height: u16,
        draw_us: u128,
        render_us: u128,
    ) {
        let profile = *self.render_profile;
        // Width changes are always useful for resize diagnosis. For ordinary
        // frames, retain only visibly slow draws to keep diagnostics compact.
        if !profile.width_changed && draw_us < 5_000 {
            return;
        }
        let session_path = self.session.path().map(Path::to_path_buf);
        let Some(debug) = self.debug.as_mut() else {
            return;
        };
        if let Err(error) = debug.write_render_timing(
            session_path.as_deref(),
            frame_width,
            frame_height,
            draw_us,
            render_us,
            profile,
        ) {
            self.debug = None;
            self.notify(
                NotifyKind::Error,
                format!("debug render logging stopped: {error}"),
            );
        }
    }

    pub(crate) fn debug_memory_line(&self) -> Option<Line<'static>> {
        let debug = self.debug.as_ref()?;
        let components = self.component_memory_json();
        let measured = components["estimated_total_bytes"].as_u64().unwrap_or(0);
        let history = components["history_bytes"].as_u64().unwrap_or(0);
        let value = |n: Option<u64>| n.map_or_else(|| "–".to_string(), format_bytes);
        Some(Line::from(format!(
            "  Debug · Total RSS {} · Heap RSS {} · Measured {} · History {}",
            value(debug.latest_rss_bytes),
            value(debug.latest_heap_bytes),
            format_bytes(measured),
            format_bytes(history),
        )))
    }

    fn component_memory_json(&self) -> serde_json::Value {
        let history_bytes = self.history.lock().map_or(0, |messages| {
            messages.capacity() * size_of::<Message>()
                + messages.iter().map(message_heap_bytes).sum::<usize>()
        });
        let turns_bytes = self.turns.capacity() * size_of::<Turn>()
            + self.turns.iter().map(turn_heap_bytes).sum::<usize>();
        let render_cache_bytes = self.frozen_render.order.capacity() * size_of::<usize>()
            + self
                .frozen_render
                .map
                .values()
                .map(|lines| {
                    lines.capacity() * size_of::<view::RenderLine>()
                        + lines.iter().map(render_line_heap_bytes).sum::<usize>()
                })
                .sum::<usize>();
        let collapsed_turn_cache = self.collapsed_turns.borrow();
        let collapsed_turn_cache_bytes = collapsed_turn_cache.retained_bytes
            + collapsed_turn_cache.map.capacity() * (size_of::<usize>() + size_of::<Arc<Turn>>());
        let visible_log_bytes = self.log_vis.capacity() * size_of::<view::VisLine>()
            + self
                .log_vis
                .iter()
                .map(|line| line.rendered.capacity())
                .sum::<usize>();
        let input_bytes = self.input.capacity()
            + self.input_stash.capacity()
            + self.kill_ring.capacity()
            + self.history_nav.iter().map(String::capacity).sum::<usize>()
            + self
                .prompt_queue
                .iter()
                .map(String::capacity)
                .sum::<usize>();
        let index_bytes = self.turn_byte_ranges.capacity() * size_of::<Option<(u64, u64)>>()
            + self.turn_event_offsets.capacity() * size_of::<Option<Vec<u64>>>()
            + self
                .turn_event_offsets
                .iter()
                .flatten()
                .map(|offsets| offsets.capacity() * size_of::<u64>())
                .sum::<usize>()
            + (self.frozen_heights.capacity() + self.frozen_heights_other_mode.capacity())
                * size_of::<usize>();
        let estimated_total = history_bytes
            + turns_bytes
            + render_cache_bytes
            + collapsed_turn_cache_bytes
            + visible_log_bytes
            + input_bytes
            + index_bytes;
        serde_json::json!({
            "history_bytes": history_bytes,
            "turns_bytes": turns_bytes,
            "render_cache_bytes": render_cache_bytes,
            "collapsed_turn_cache_bytes": collapsed_turn_cache_bytes,
            "collapsed_turn_cache_entries": collapsed_turn_cache.map.len(),
            "visible_log_bytes": visible_log_bytes,
            "input_and_queue_bytes": input_bytes,
            "indexes_bytes": index_bytes,
            "estimated_total_bytes": estimated_total,
        })
    }
}

impl DebugState {
    fn create() -> std::io::Result<Self> {
        let mut debug_dir = lofi_core::state::ensure_state_dir()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        debug_dir.push("debug");
        std::fs::create_dir_all(&debug_dir)?;
        Ok(Self {
            file: None,
            path: None,
            session_path: None,
            debug_dir,
            started: Instant::now(),
            latest_rss_bytes: None,
            latest_heap_bytes: None,
            previous_sample: None,
        })
    }

    fn ensure_file(&mut self, session_path: Option<&Path>) -> std::io::Result<()> {
        let session_path = session_path.map(Path::to_path_buf);
        if self.file.is_some() && self.session_path == session_path {
            return Ok(());
        }
        let Some(name) = session_path.as_ref().and_then(|path| path.file_name()) else {
            // Fresh sessions are created lazily on the first prompt. Keep
            // diagnostics armed and wait rather than inventing an unrelated id.
            self.file = None;
            self.path = None;
            self.session_path = None;
            return Ok(());
        };
        let path = self.debug_dir.join(name);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.file = Some(file);
        self.path = Some(path);
        self.session_path = session_path;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_render_timing(
        &mut self,
        session_path: Option<&Path>,
        frame_width: u16,
        frame_height: u16,
        draw_us: u128,
        render_us: u128,
        profile: RenderProfile,
    ) -> std::io::Result<()> {
        self.ensure_file(session_path)?;
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let record = serde_json::json!({
            "schema_version": 1,
            "timestamp_ms": timestamp_ms,
            "elapsed_ms": self.started.elapsed().as_millis(),
            "event": "render_timing",
            "frame": {
                "width": frame_width,
                "height": frame_height,
                "draw_us": draw_us,
                "render_callback_us": render_us,
                "backend_us": draw_us.saturating_sub(render_us),
            },
            "resize": {
                "events": profile.resize_events,
                "batch_us": profile.resize_batch_us,
                "quiet_us": profile.resize_quiet_us,
            },
            "log": {
                "width": profile.width,
                "height": profile.height,
                "turns": profile.turns,
                "frozen_turns": profile.frozen_turns,
                "width_changed": profile.width_changed,
                "total_us": profile.log_total_us,
                "ensure_frozen_us": profile.ensure_frozen_us,
                "live_height_us": profile.live_height_us,
                "viewport_cache_us": profile.viewport_cache_us,
                "frozen_window_us": profile.frozen_window_us,
                "live_window_us": profile.live_window_us,
            },
        });
        serde_json::to_writer(&mut *file, &record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn write_sample(&mut self, app: &App, event: &str) -> std::io::Result<()> {
        self.ensure_file(app.session.path())?;
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let process = read_process_memory().unwrap_or_default();
        let allocator = read_allocator_memory();
        let components = app.component_memory_json();
        let component_bytes = components["estimated_total_bytes"].as_u64().unwrap_or(0);
        let totals = SampleTotals {
            rss: process.rss_bytes,
            private_dirty: process.private_dirty_bytes,
            anonymous: process.anonymous_bytes,
            allocator_allocated: allocator.allocated,
            components: component_bytes,
        };
        let previous = self.previous_sample.replace(totals);
        self.latest_rss_bytes = (process.rss_bytes > 0).then_some(process.rss_bytes);
        self.latest_heap_bytes =
            (process.mappings.heap.rss > 0).then_some(process.mappings.heap.rss);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let history_messages = app.history.lock().map_or(0, |messages| messages.len());
        let transcript_bytes = app
            .session
            .cursor
            .as_ref()
            .map_or(0, store::SessionCursor::len);
        let context_tokens = app.status_usage.map_or(0, |usage| {
            usage.input_tokens
                + usage.output_tokens
                + usage.cache_read_tokens
                + usage.cache_write_tokens
        });
        let record = serde_json::json!({
            "schema_version": 1,
            "timestamp_ms": timestamp_ms,
            "elapsed_ms": self.started.elapsed().as_millis(),
            "event": event,
            "process": {
                "rss_bytes": process.rss_bytes,
                "rss_peak_bytes": process.rss_peak_bytes,
                "virtual_bytes": process.virtual_bytes,
                "rss_anon_bytes": process.rss_anon_bytes,
                "rss_file_bytes": process.rss_file_bytes,
                "swap_bytes": process.swap_bytes,
                "pss_bytes": process.pss_bytes,
                "pss_anon_bytes": process.pss_anon_bytes,
                "pss_file_bytes": process.pss_file_bytes,
                "pss_shmem_bytes": process.pss_shmem_bytes,
                "private_clean_bytes": process.private_clean_bytes,
                "private_dirty_bytes": process.private_dirty_bytes,
                "shared_clean_bytes": process.shared_clean_bytes,
                "shared_dirty_bytes": process.shared_dirty_bytes,
                "anonymous_bytes": process.anonymous_bytes,
                "lazy_free_bytes": process.lazy_free_bytes,
                "anon_huge_pages_bytes": process.anon_huge_pages_bytes,
                "mappings": {
                    "executable": mapping_json(&process.mappings.executable),
                    "shared_libraries": mapping_json(&process.mappings.shared_libraries),
                    "heap": mapping_json(&process.mappings.heap),
                    "stacks": mapping_json(&process.mappings.stacks),
                    "anonymous": mapping_json(&process.mappings.anonymous),
                    "other_files": mapping_json(&process.mappings.other_files),
                    "kernel": mapping_json(&process.mappings.kernel),
                },
                "threads": process.threads,
                "cpu_user_ticks": process.cpu_user_ticks,
                "cpu_system_ticks": process.cpu_system_ticks,
            },
            "retention": {
                "measured_component_bytes": component_bytes,
                "private_dirty_minus_measured_bytes": signed_delta(process.private_dirty_bytes, component_bytes),
                "anonymous_minus_measured_bytes": signed_delta(process.anonymous_bytes, component_bytes),
                "rss_minus_measured_bytes": signed_delta(process.rss_bytes, component_bytes),
            },
            "allocator": {
                "allocated_bytes": allocator.allocated,
                "free_bytes": allocator.free,
                "arena_bytes": allocator.arena,
                "mmap_bytes": allocator.mmap,
                "releasable_bytes": allocator.releasable,
            },
            "delta_from_previous": previous.map(|previous| serde_json::json!({
                "rss_bytes": signed_delta(totals.rss, previous.rss),
                "private_dirty_bytes": signed_delta(totals.private_dirty, previous.private_dirty),
                "anonymous_bytes": signed_delta(totals.anonymous, previous.anonymous),
                "allocator_allocated_bytes": option_delta(totals.allocator_allocated, previous.allocator_allocated),
                "component_bytes": signed_delta(totals.components, previous.components),
            })),
            "components": components,
            "context": {
                "run_active": app.run.is_some(),
                "model": app.session_model(),
                "session_file": app.session.path().map(|path| path.display().to_string()),
                "transcript_bytes": transcript_bytes,
                "history_messages": history_messages,
                "turns": app.turns.len(),
                "resident_turn_blocks": app.turns.iter().filter(|turn| !turn.blocks.is_empty()).count(),
                "render_cache_entries": app.frozen_render.map.len(),
                "visible_log_lines": app.log_vis.len(),
                "queued_prompts": app.prompt_queue.len(),
                "context_tokens": context_tokens,
                "context_limit": app.ctx_limit,
                "compacted": app.compacted,
                "verbose": app.verbose,
            }
        });
        serde_json::to_writer(&mut *file, &record)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

fn signed_delta(current: u64, previous: u64) -> i128 {
    i128::from(current) - i128::from(previous)
}

fn option_delta(current: Option<u64>, previous: Option<u64>) -> Option<i128> {
    Some(signed_delta(current?, previous?))
}

fn read_allocator_memory() -> AllocatorMemory {
    AllocatorMemory::default()
}

fn message_heap_bytes(message: &Message) -> usize {
    message.blocks.capacity() * size_of::<ContentBlock>()
        + message.blocks.iter().map(content_heap_bytes).sum::<usize>()
}

fn content_heap_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.capacity(),
        ContentBlock::ToolUse { id, name, input } => {
            id.capacity() + name.capacity() + json_heap_bytes(input)
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => tool_use_id.capacity() + content.capacity(),
        ContentBlock::Thinking { text, signature } => {
            text.capacity() + signature.as_ref().map_or(0, String::capacity)
        }
    }
}

fn json_heap_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => value.capacity(),
        serde_json::Value::Array(values) => {
            values.capacity() * size_of::<serde_json::Value>()
                + values.iter().map(json_heap_bytes).sum::<usize>()
        }
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.capacity() + json_heap_bytes(value))
            .sum(),
        _ => 0,
    }
}

fn turn_heap_bytes(turn: &Turn) -> usize {
    turn.prompt.capacity()
        + turn.blocks.capacity() * size_of::<Block>()
        + turn.blocks.iter().map(block_heap_bytes).sum::<usize>()
}

fn block_heap_bytes(block: &Block) -> usize {
    match block {
        Block::Text(text) | Block::Error(text) => text.capacity(),
        Block::Thinking(thinking) => thinking.text.capacity(),
        Block::Tool(tool) => {
            tool.id.capacity()
                + tool.name.capacity()
                + tool.input.capacity()
                + tool.label.as_ref().map_or(0, String::capacity)
                + tool.result.as_ref().map_or(0, String::capacity)
                + tool.native.capacity() * size_of::<NativeTool>()
                + tool
                    .native
                    .iter()
                    .map(|native| {
                        native.name.capacity()
                            + native.args.capacity()
                            + native.result.as_ref().map_or(0, String::capacity)
                    })
                    .sum::<usize>()
        }
        Block::UserBash {
            command, output, ..
        } => command.capacity() + output.capacity(),
        Block::TurnEnd { label, .. } => label.capacity(),
        Block::TurnFailed { label, error, .. } => label.capacity() + error.capacity(),
        Block::Compaction { summary, .. } => summary.capacity(),
    }
}

fn render_line_heap_bytes(line: &view::RenderLine) -> usize {
    line.line
        .spans
        .iter()
        .map(|span| match &span.content {
            std::borrow::Cow::Owned(text) => text.capacity(),
            std::borrow::Cow::Borrowed(_) => 0,
        })
        .sum()
}

fn mapping_json(mapping: &MappingMemory) -> serde_json::Value {
    serde_json::json!({
        "rss_bytes": mapping.rss,
        "pss_bytes": mapping.pss,
        "private_dirty_bytes": mapping.private_dirty,
    })
}

fn mapping_bucket<'a>(breakdown: &'a mut MappingBreakdown, path: &str) -> &'a mut MappingMemory {
    if path == "[heap]" {
        &mut breakdown.heap
    } else if path.starts_with("[stack") {
        &mut breakdown.stacks
    } else if matches!(path, "[vdso]" | "[vvar]" | "[vsyscall]") {
        &mut breakdown.kernel
    } else if path.is_empty() {
        &mut breakdown.anonymous
    } else if std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("so"))
        || path.contains(".so.")
    {
        &mut breakdown.shared_libraries
    } else if path.starts_with('/') {
        &mut breakdown.other_files
    } else {
        &mut breakdown.anonymous
    }
}

fn read_mapping_breakdown() -> std::io::Result<MappingBreakdown> {
    let smaps = std::fs::read_to_string("/proc/self/smaps")?;
    let executable = std::env::current_exe().ok();
    let mut breakdown = MappingBreakdown::default();
    let mut current_path = String::new();
    for line in smaps.lines() {
        if line.as_bytes().first().is_some_and(u8::is_ascii_hexdigit)
            && line
                .split_whitespace()
                .next()
                .is_some_and(|field| field.contains('-'))
        {
            current_path = line.split_whitespace().nth(5).unwrap_or("").to_string();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(key, "Rss" | "Pss" | "Private_Dirty") {
            continue;
        }
        let Some(kib) = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let is_executable = executable.as_ref().is_some_and(|exe| {
            current_path == exe.to_string_lossy()
                || current_path.strip_suffix(" (deleted)") == Some(exe.to_string_lossy().as_ref())
        });
        let bucket = if is_executable {
            &mut breakdown.executable
        } else {
            mapping_bucket(&mut breakdown, &current_path)
        };
        match key {
            "Rss" => bucket.rss += kib * 1024,
            "Pss" => bucket.pss += kib * 1024,
            "Private_Dirty" => bucket.private_dirty += kib * 1024,
            _ => {}
        }
    }
    Ok(breakdown)
}

fn read_process_memory() -> std::io::Result<ProcessMemory> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let mut memory = ProcessMemory {
        mappings: read_mapping_breakdown().unwrap_or_default(),
        ..ProcessMemory::default()
    };
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let number = value
            .split_whitespace()
            .next()
            .and_then(|value| value.parse().ok());
        match (key, number) {
            ("VmRSS", Some(value)) => memory.rss_bytes = value * 1024,
            ("VmHWM", Some(value)) => memory.rss_peak_bytes = value * 1024,
            ("VmSize", Some(value)) => memory.virtual_bytes = value * 1024,
            ("RssAnon", Some(value)) => memory.rss_anon_bytes = value * 1024,
            ("RssFile", Some(value)) => memory.rss_file_bytes = value * 1024,
            ("VmSwap", Some(value)) => memory.swap_bytes = value * 1024,
            ("Threads", Some(value)) => memory.threads = value,
            _ => {}
        }
    }
    if let Ok(rollup) = std::fs::read_to_string("/proc/self/smaps_rollup") {
        for line in rollup.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let Some(kib) = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            let bytes = kib * 1024;
            match key {
                "Pss" => memory.pss_bytes = bytes,
                "Pss_Anon" => memory.pss_anon_bytes = bytes,
                "Pss_File" => memory.pss_file_bytes = bytes,
                "Pss_Shmem" => memory.pss_shmem_bytes = bytes,
                "Private_Clean" => memory.private_clean_bytes = bytes,
                "Private_Dirty" => memory.private_dirty_bytes = bytes,
                "Shared_Clean" => memory.shared_clean_bytes = bytes,
                "Shared_Dirty" => memory.shared_dirty_bytes = bytes,
                "Anonymous" => memory.anonymous_bytes = bytes,
                "LazyFree" => memory.lazy_free_bytes = bytes,
                "AnonHugePages" => memory.anon_huge_pages_bytes = bytes,
                _ => {}
            }
        }
    }
    if let Some((_, suffix)) = stat.rsplit_once(") ") {
        let fields: Vec<&str> = suffix.split_whitespace().collect();
        memory.cpu_user_ticks = fields
            .get(11)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        memory.cpu_system_ticks = fields
            .get(12)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
    }
    Ok(memory)
}
