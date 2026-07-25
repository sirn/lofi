//! Opt-in process and component memory diagnostics for the TUI.

use super::*;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::mem::size_of;
use std::time::{SystemTime, UNIX_EPOCH};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

pub(super) struct DebugState {
    file: File,
    path: PathBuf,
    started: Instant,
    last_sample: Option<Instant>,
    latest_rss_bytes: Option<u64>,
}

#[derive(Default)]
struct ProcessMemory {
    rss_bytes: u64,
    rss_peak_bytes: u64,
    virtual_bytes: u64,
    rss_anon_bytes: u64,
    rss_file_bytes: u64,
    swap_bytes: u64,
    threads: u64,
    cpu_user_ticks: u64,
    cpu_system_ticks: u64,
}

impl App {
    pub(super) fn toggle_debug(&mut self) {
        if let Some(debug) = self.debug.take() {
            let path = debug.path.display().to_string();
            self.notify(NotifyKind::Info, format!("debug logging disabled · {path}"));
            return;
        }
        match DebugState::create() {
            Ok(debug) => {
                let path = debug.path.display().to_string();
                self.debug = Some(debug);
                self.debug_sample("enabled", true);
                self.notify(NotifyKind::Info, format!("debug logging enabled · {path}"));
            }
            Err(e) => self.notify(NotifyKind::Error, format!("enable debug logging: {e}")),
        }
    }

    pub(super) fn debug_sample(&mut self, event: &str, force: bool) {
        let Some(mut debug) = self.debug.take() else {
            return;
        };
        if !force
            && debug
                .last_sample
                .is_some_and(|last| last.elapsed() < SAMPLE_INTERVAL)
        {
            self.debug = Some(debug);
            return;
        }
        if let Err(e) = debug.write_sample(self, event) {
            self.notify(NotifyKind::Error, format!("debug logging stopped: {e}"));
        } else {
            self.debug = Some(debug);
        }
    }

    pub(crate) fn debug_badge(&self) -> Option<String> {
        self.debug.as_ref().map(|debug| {
            debug.latest_rss_bytes.map_or_else(
                || "DEBUG".to_string(),
                |rss| format!("DEBUG {}", format_bytes(rss)),
            )
        })
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
            + self.frozen_heights.capacity() * size_of::<usize>();
        let estimated_total = history_bytes
            + turns_bytes
            + render_cache_bytes
            + visible_log_bytes
            + input_bytes
            + index_bytes;
        serde_json::json!({
            "history_bytes": history_bytes,
            "turns_bytes": turns_bytes,
            "render_cache_bytes": render_cache_bytes,
            "visible_log_bytes": visible_log_bytes,
            "input_and_queue_bytes": input_bytes,
            "indexes_bytes": index_bytes,
            "estimated_total_bytes": estimated_total,
        })
    }
}

impl DebugState {
    fn create() -> std::io::Result<Self> {
        let mut dir = lofi_core::state::ensure_state_dir()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        dir.push("debug");
        std::fs::create_dir_all(&dir)?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let path = dir.join(format!("{millis}-{}.jsonl", std::process::id()));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            started: Instant::now(),
            last_sample: None,
            latest_rss_bytes: None,
        })
    }

    fn write_sample(&mut self, app: &App, event: &str) -> std::io::Result<()> {
        let process = read_process_memory().unwrap_or_default();
        self.latest_rss_bytes = (process.rss_bytes > 0).then_some(process.rss_bytes);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let history_messages = app.history.lock().map_or(0, |messages| messages.len());
        let transcript_bytes = app
            .session
            .path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map_or(0, |metadata| metadata.len());
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
                "threads": process.threads,
                "cpu_user_ticks": process.cpu_user_ticks,
                "cpu_system_ticks": process.cpu_system_ticks,
            },
            "components": app.component_memory_json(),
            "context": {
                "run_active": app.run.is_some(),
                "model": app.session_model(),
                "session_file": app.session.path.as_ref().map(|path| path.display().to_string()),
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
        serde_json::to_writer(&mut self.file, &record)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.last_sample = Some(Instant::now());
        Ok(())
    }
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

fn read_process_memory() -> std::io::Result<ProcessMemory> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let mut memory = ProcessMemory::default();
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
