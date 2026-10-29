# lofi API reference

The agent's single tool is `exec`. Inside `exec` code, call async functions on
the global `lofi` object. Use `await`. All file paths are resolved against the
workspace root. Read-only tools also accept absolute paths under registered roots, such as `lofi.tmpDir` and skill directories. Relative paths that escape the workspace root are rejected. An absolute path outside every allowed root asks the user to approve its directory for reads and writes; the approval covers the directory and its subdirectories and is remembered across sessions. Use an absolute path to reach an approved directory.

## lofi.read(path, opts?)

Read a file as UTF-8 with optional line range, or attach an image.

**Parameters:**
- `path` (string, required) — a workspace-relative file path, or an absolute path under a registered read root. Tilde (`~`) is not expanded.
- `opts` (object, optional) — `{ offset?, limit? }`. `offset` is a 1-indexed line to start from (default 1); `limit` selects at most that many source lines. Omit `limit` to read from `offset` to the end before visible-output truncation.

**Returns (text):** `{ ok, content, start_line, total_lines, truncated, next_offset }`.
`content` is head-truncated to the configured `[truncate] max_lines` and `max_bytes` values (defaults: 2000 lines and 50 KiB).
`start_line` is the 1-indexed first line returned. `total_lines` is the file's
line count. `truncated` is true when more lines remain below either cap.

`next_offset` is the paging cursor: pass it as `offset` to read the next
page. It is null when the file is fully read. The cursor always advances; a
single line larger than `max_bytes` cannot be returned and is skipped. Page
a whole file with:

```
let offset = 1;
while (true) {
    const page = await lofi.read(path, { offset });
    // ... use page.content ...
    if (page.next_offset === null) break;
    offset = page.next_offset;
}
```

**Returns (image):** for a path with an image extension (`.png`, `.jpg`,
`.jpeg`, `.gif`, `.webp`, `.bmp`), returns `{ ok, type: "image", media_type,
data_b64 }` instead. The host decodes `data_b64` and attaches the image to the
conversation as vision input, so reading an image file is how you look at it.
The image path ignores `offset`/`limit`.

## lofi.ls(dir?)

List entries under a directory.

**Parameters:**
- `dir` (string, optional) — a workspace-relative directory or an absolute directory under a registered read root. Empty or `.` means the workspace root.

**Returns:** `{ ok, entries }` — a sorted array of relative paths. The result
is always complete: if the directory has more than 50,000 entries the call
throws (narrow with a more specific `dir`).

## lofi.find(glob, dir?, filtered?)

Recursive glob match.

**Parameters:**
- `glob` (string, required) — glob pattern.
- `dir` (string, optional) — a workspace-relative directory or an absolute directory under a registered read root (default: workspace root).
- `filtered` (boolean, optional, default `true`) — when `true`, files ignored
  by `.gitignore`/`.ignore` and hidden (dot) files are pruned from the walk,
  like `rg`/`fd`. Pass `false` to traverse every file.

**Returns:** `{ ok, matches }` — a sorted array of relative paths. The call
throws if it exceeds 50,000 matches or 65,536 traversed entries (narrow the
`glob` or `dir`).

## lofi.grep(pattern, path?)

Search file contents with a regex.

**Parameters:**
- `pattern` (string or object, required) — a regex string, or
  `{ regex, ic?, ctx?, filtered? }` (`ic` = case-insensitive, `ctx` = context
  lines around each match, `filtered` = prune ignored/hidden files, default
  `true`).
- `path` (string, optional) — a workspace-relative file or directory, or an absolute path under a registered read root (default: workspace root).

**Returns:** `{ ok, matches, skipped }`. `matches` is an array of
`{ file, line, content, matched }` (`matched` is false for context lines).
`skipped: { oversize, unreadable }` counts files skipped for being over 8 MB
or unreadable. The call throws if it exceeds 65,536 files traversed, 10,000
matching/context rows, or 8 MB of total output (narrow the `path` or `regex`).

## lofi.write({ path, text })

Write a file, creating parent directories as needed.

**Parameters:**
- `path` (string, required) — file path relative to workspace root.
- `text` (string, required) — content to write.

**Returns:** `{ ok, content }` — `content` echoes the text written.

## lofi.edit({ path, old, new })

Replace a single occurrence of text in a file.

**Parameters:**
- `path` (string, required) — file path relative to workspace root.
- `old` (string, required) — the text to find.
- `new` (string, required) — the replacement text.

**Returns:** `{ ok, old, new }` — `old`/`new` echo the replaced and replacement
text. Errors if `old` is absent or appears more than once.

## lofi.patch({ path, patch })

Apply a unified-diff patch to `path`. Use when an edit has several discontiguous
changes — the hunks locate their targets by context, not by exact string match.

**Parameters:**
- `path` (string, required) — file path relative to workspace root.
- `patch` (string, required) — unified-diff body. `--- a/`/`+++ b/` headers
  are optional; bare `@@` hunk blocks work. Context lines are prefixed with
  a space, removals with `-`, additions with `+`. Each hunk must apply in
  order; a modest context window tolerates line-number drift.

**Returns:** `{ ok, path, hunks }` — number of hunks applied. Errors if the
patch is malformed, a hunk's context is not found, or hunks are out of order.

## lofi.bash({ cmd, timeoutMs?, steer? })

Run a shell command with `sh -c`, cwd pinned to the workspace root. Stdout and
stderr are merged.

**Parameters:**
- `cmd` (string, required) — the shell command.
- `timeoutMs` (number, optional) — timeout in milliseconds (default 120000).
- `steer` (boolean, optional) — set `false` to run the command without
  steering refusals (default `true`).

**Returns:** `{ ok, output, code, command, directory, signal, duration_ms,
status }`. `code` is the exit status (null on signal/timeout); `signal` is the
Unix signal number (null unless killed by a signal); `duration_ms` is wall
time; `status` is `"exited"`, `"signaled"`, or `"timeout"` after execution. A command blocked before execution returns `"denied"` or `"needs_confirmation"`, with `ok: false` and no exit code. Output is tail-truncated to the configured `[truncate] max_lines` and `max_bytes` values (defaults: 2000 lines and 50 KiB), keeping the end where errors land. When truncated, the full output is saved to a file under `lofi.tmpDir`; the notice names it so you can page through it with `lofi.read(path)`. The child env is
stripped to a minimal baseline by default; env vars the user approved are
present but their values are replaced with `[redacted]` in the output.

**Steering:** by default the tool refuses a command that is only a sleep
call, or whose program word is the `claude` or `codex` CLI, before the run.
Detection inspects the command's leading words only, so a CLI nested later in
the command (for example after `&&`) is not refused. The refusal
names the tool to use instead with one example call and the `steer: false`
opt-out: `lofi.sleep` for a short wait, `notifyAt` to end the turn on a
long pause (`notifyEvery` for a repeating one), or `lofi.subagent` for an
agent call. With `steer: false` the command runs without steering. Bare `&`
backgrounding is always refused with no opt-out: the refusal names
`jobSpawn`, and `steer: false` drops that suggestion but keeps the refusal.
Matching uses the parsed command, not a substring: leading `NAME=value`
words and argv wrappers (`env`, `sudo`, `nice`, ...) are unwrapped, so
`env claude` is refused like `claude` and `sleep -- 5` is refused like
`sleep 5`. A wrapper that takes a shell program (`sh -c '...'`) is not
matched through. Configured policy rules always apply. Steering is a
`bash` tool behavior: `jobSpawn` runs its command without steering and
still refuses bare `&`, dropping the `jobSpawn` suggestion because the
caller already uses it.

## lofi.jobSpawn({ cmd, tty?, cols?, rows?, idleMs?, timeoutMs?, notify?, notifyIntervalMs? })

Start a background shell command without blocking the current turn. The
command runs `sh -c` from the workspace root in its own process group with
the same shell policy, stripped environment, and output redaction as
`lofi.bash`. Returns immediately.

Use the default completion notification instead of polling for the job to
finish: the notice arrives as a new user message once your turn ends, so end
the turn rather than sleep or poll. Inspect state and output with `jobStatus`
and `jobRead`.

**Parameters:**
- `cmd` (string, required) — the shell command.
- `tty` (boolean, optional, default `false`) — run the child on a
  pseudo-terminal instead of plain pipes. Interactive programs can then be
  driven with `jobType` and `jobKeyPress`. The child gets its
  own session and controlling terminal, so Ctrl+C reaches it as SIGINT and
  `/dev/tty` works.
- `cols`, `rows` (number, optional, defaults `120`, `40`) — the PTY window size. Each value is clamped to 1–1000. Only meaningful when `tty: true`.
- `idleMs` (number, optional) — output-idle threshold. Clamped to a 500 ms
  floor. Defaults to 15000 for tty jobs; non-tty jobs default to no idle
  detection because silence is normal compute. When the output is unchanged
  for this long, the job becomes `idle` and queues one idle notice.
- `timeoutMs` (number, optional) — kill deadline in milliseconds. There is
  no default: background jobs are the long-running builds and test runs
  that do not fit a synchronous tool call, so a job runs until it exits, is
  killed, or the session ends. Pass `timeoutMs` to cap it; on timeout the
  whole process group is killed and the job ends as `"timed_out"`.
- `notify` (boolean, optional, default `true`) — master switch for the
  terminal, idle, and periodic notices. `false` silences the job entirely.
- `notifyIntervalMs` (number, optional) — turn on periodic progress pings
  while the job runs. Clamped to a 5000 ms floor. Passing it implies
  `notify: true`; a separate `jobNotify` call is not needed.

**Returns:** `{ ok, id, state, command, directory, pid, timeoutMs,
logPath, tty, cols, rows, idleMs }`. `state` starts as `"running"`.
`logPath` names the merged stdout/stderr log under `lofi.tmpDir`; it is a
read root, so `lofi.read` on it also works. For a tty job the driver copies
PTY output into this log. The log grows unbounded for the life of the job
and is removed with the session; page through it with `jobRead` rather than
reading it whole.

## lofi.jobStatus({ id })

Current state, timestamps, exit status, and limits for a job.

**Returns:** `{ ok, id, state, command, directory, pid, exitCode, signal,
durationMs, timeoutMs, logPath, notify, notifyIntervalMs, notifyChanged,
tty, cols, rows, idle, idleMs, idleForMs }`. `state` is `"running"`,
`"completed"`, `"failed"`, `"cancelled"`, or `"timed_out"`. `idle` is true
once output has been unchanged for `idleMs`; `idleForMs` is how long it has
been idle. Unknown ids return `{ ok: false, error }`.

## lofi.jobList()

List every job this session spawned, newest first. Session-scoped: jobs
from other sessions are not visible.

**Returns:** `{ ok, jobs }` where each entry is the same shape as
`jobStatus`. The list is empty when no jobs have been spawned.

## lofi.jobRead({ id, cursor?, limit? })

Incremental read of a job's merged stdout/stderr log.

**Parameters:**
- `id` (string, required) — job id from `jobSpawn`.
- `cursor` (number, optional) — byte offset to resume from (default 0).
- `limit` (number, optional) — max bytes to return (default and max 65536).

**Returns:** `{ ok, id, state, cursor, totalBytes, output, done }`.
`cursor` is the next offset to pass. `output` is redacted. `done` is true
once the job is terminal.

## lofi.jobScreen({ id })

Read the current visible screen of a tty job after terminal control sequences
have been applied. Use this to observe a full-screen or interactive program
before sending more input.

**Parameters:**
- `id` (string, required) — tty job id from `jobSpawn`.

**Returns:** `{ ok, id, cols, rows, lines }`. `lines` contains one redacted
string per terminal row with trailing blank cells removed. Plain jobs return
`{ ok: false, id, error }` because they have no terminal screen.

## lofi.jobKill({ id, reason? })

Cancel a job: SIGKILL its entire process group (children and grandchildren)
and mark it `"cancelled"`. Idempotent — killing an already-terminal job is
a no-op that returns its current status.

**Returns:** the same shape as `jobStatus`, plus `reason`.

## lofi.jobNotify({ id, enabled?, intervalMs?, changed?, idleMs? })

Configure notifications for a job that did not opt in at spawn. Notices are
one-line messages the agent injects at the next round boundary (and the UI
surfaces live).

**Parameters:**
- `id` (string, required) — job id from `jobSpawn`.
- `enabled` (boolean, optional) — master switch. Defaults to `true` when
  omitted: calling `jobNotify` at all means "notify me". `enabled: false`
  silences the job, including the terminal notice.
- `intervalMs` (number, optional) — turn on periodic progress pings while
  the job runs. Clamped to a 5000 ms floor. When omitted and the job has no
  interval yet, enabling sets it to the 30000 ms default.
- `changed` (boolean, optional, default `true`) — when true, a periodic
  tick only emits if the log grew since the last tick, so a live-but-silent
  job stays quiet. `false` emits every tick while running.
- `idleMs` (number, optional) — output-idle threshold. Clamped to a 500 ms
  floor.

The terminal transition always queues one notice when `enabled`, regardless
of how `changed` treated the intermediate ticks.

**Returns:** `{ ok, id, notify, intervalMs, changed, idleMs }`.

## lofi.jobType({ id, text })

Write literal bytes to a tty job's PTY input. Use this to answer a prompt;
include a trailing newline or follow it with `jobKeyPress({ key: "Enter" })`.
Errors with `ok: false` when the job is not a tty job.

`text` is limited to 64 KiB per call.

**Returns:** `{ ok, id, sent }`.

## lofi.jobKeyPress({ id, key })

Send one named key to a tty job. `key` accepts the `tu` names: `Enter`,
`Return`, `Tab`, `Escape`, `Backspace`, `Delete`, `Insert`, `Up`, `Down`,
`Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Space`, `Ctrl+C`,
`Ctrl+D`, `Ctrl+Z`, `Ctrl+U`, `Ctrl+L`, `Ctrl+A`, `Ctrl+E`, and `F1`–`F12`.
Errors with `ok: false` when the key is unknown or the job is not a tty job.

**Returns:** `{ ok, id, key }`.

## lofi.sleep(seconds)

Wait inside the exec for `seconds` seconds (a number; fractions are fine). The
wait observes run cancellation, so a cancelled turn leaves it at once. `seconds`
must be at most 86400 (one day): for a longer pause, schedule a wake-up with
`notifyAt` and end the turn instead of holding the exec open. Do not sleep to
wait for a background job: its completion notice arrives as a new user message
once the turn ends.

**Parameters:**
- `seconds` (number, required) — how long to wait, in seconds. `0` resolves
  at once.

**Returns:** `{ ok }`. Throws when `seconds` is not a non-negative number
within the cap, or when the run is cancelled mid-wait.

## lofi.notifyAt(at, message?)

Queue a one-shot wake-up notice for the session. When `at` passes, the host
delivers the notice as a notice prompt — the same path a finished background
job's notice takes — so the agent wakes to it whether or not it is mid-turn.
An `at` in the past fires on the next host poll.

Nothing is persisted: the timer lives and dies with the session, and a `/new`
session drops pending timers.

**Parameters:**
- `at` (string, required) — an RFC 3339 timestamp with a timezone offset,
  e.g. `2026-01-01T09:00:00+00:00` or `2026-01-01T09:00:00Z`.
- `message` (string, optional) — carried in the notice as
  `notifyAt fired: <message>`. Without one the notice reads `notifyAt fired`.
  Limited to 500 characters.

**Returns:** `{ ok, at, message }`. Throws when `at` is not a parseable RFC
3339 timestamp or `message` is not a string within the limit.

## lofi.notifyEvery(every, message?)

Like `notifyAt`, but re-armed: the first notice comes one `every` duration
from now and each interval after it. A host that polls late gets one notice
per elapsed window, never a burst of the missed ones.

**Parameters:**
- `every` (string, required) — a duration like `5m`, `1h30m`, or `500ms`.
  Every segment carries a unit: `ms`, `s`, `m`, `h`, or `d`. At least 1s.
- `message` (string, optional) — carried in each notice as
  `notifyEvery fired: <message>`. Without one the notice reads
  `notifyEvery fired`. Limited to 500 characters.

**Returns:** `{ ok, every, message }`. Throws when `every` is not a parseable
duration at least 1s, or `message` is not a string within the limit.

## lofi.notifyCancel()

Drop every pending `notifyAt` and `notifyEvery` of this caller. A subagent
cancels its own timers, not the session's.

**Returns:** `{ ok, cancelled }` with the number of timers dropped.

## lofi.tmpDir

A string property: the absolute path to the per-session tmp directory backing
`lofi.bash` full-output logs. This directory is a read root — `lofi.read`,
`lofi.ls`, `lofi.find`, and `lofi.grep` can access files under it.

## lofi.memory()

Read the workspace memory: a small state file shared by every session opened
in the same workspace, injected into each compaction handoff.

**Returns:** an object with `path` (string) — the absolute location of the
memory file — and `text` (string) — its current content, an empty string
when no memory exists. Update the file with the normal tools
(`lofi.write`, `lofi.edit`, `lofi.patch`): the path is a registered
root they accept for read and write without approval. Rewrite the whole file
rather than appending, and keep it within 3000 characters — a compaction
clips the file at that limit. Keep session-specific progress in the
conversation; the memory carries state every session in the workspace needs.

## lofi.recall({ query?, scope?, page?, expand? })

Search the full session transcript (including messages a compaction folded
away) and return rendered matches as text.

**Parameters:**
- `query` (string, optional) — a regex or multi-word BM25 query. Omit to
  browse the most recent entries.
- `scope` (string, optional) — `"lineage"` (default, active branch), `"all"` (whole session), or `"compaction:N"` / `"compaction:latest"` (within one compaction's summarized range). `"latest"` is a short form for `"compaction:latest"`.
- `page` (number, optional) — 1-based page number.
- `expand` (array, optional) — indices of entries to return with full
  untruncated content.

**Returns:** `{ text, status }`.

## lofi.result(eventId)

Recover the original retained content of a tool result or tool call that
compaction elided. Pathological legacy values above the current result limit are
bounded during recovery.

**Parameters:**
- `eventId` (string, required) — the event id shown in a cleared stub (`[id]`).

**Returns:** `string` — the raw content (a tool result's output text, or a
tool call's input JSON). Cheap and on-demand.

## lofi.subagent({ prompt, id?, model?, thinking?, system?, timeout_ms?, output_limit?, token_budget? })

Delegate a task to a foreground subagent: a fresh agent run in its own
context window. Only the final result crosses back into this exec, so use
it for work whose intermediate output you do not need (broad searches,
log reads, mechanical edits).

**Parameters:**
- `prompt` (string, required) — the task for the subagent.
- `id` (string, optional) — resume a previous subagent run, continuing its
  context with this prompt. `model`, `thinking`, and `system` are rejected
  on resume; `timeout_ms`, `output_limit`, and `token_budget` still apply.
  A resumed run keeps the model and service tier it first ran with while
  that model and tier are still available; otherwise it uses the session
  model.
- `model` (string, optional) — model query (e.g. `anthropic/claude-haiku-4.5`)
  overriding the session model. List choices with `lofi.models()`. The run
  keeps the session service tier when it is not `auto` and the new model
  declares it (see `service_tiers` in `lofi.models()`); otherwise the
  normal default order applies (model, provider, then the configured
  `[agent]` tier). A `@tier` suffix in `model` always wins.
- `thinking` (string, optional) — thinking level (`off`, `low`, `medium`,
  `high`, `xhigh`).
- `system` (string, optional) — system prompt override.
- `timeout_ms` (number, optional) — wall-clock limit for the whole run.
- `output_limit` (number, optional) — cap on the returned result bytes.
- `token_budget` (number, optional) — total token budget, counting every
  token category the provider reports (input, output, cache). The run is
  cancelled and fails once its spend crosses the budget.

**Returns:** `{ ok, id, status, result?, error?, session_id?, truncated?, full_bytes?, usage? }`.
`status` is one of `completed`, `failed`, `cancelled`, `timed_out` —
`running` when a resume targets a run that is still active, or
`unavailable` when the sandbox has no subagent support. Keep the `id` to
resume the run later; `/new` or capacity eviction invalidates it. Entries
survive a process restart when the session persists: at the start of the
next turn the registry rehydrates from this session's transcripts, and a
resume with the same `id` replays the stored history. A run whose
transcript has no settle marker — the process exited mid-run — comes back
as `cancelled` with the error "lofi exited before the run settled".
`session_id` is the id of the persisted transcript, present unless the
session does not persist; the transcript stays on disk even after the `id`
is evicted. When `result` was cut to fit `output_limit`, `truncated` is
`true` and `full_bytes` holds the full length. The retained result and
error are capped at 32768 bytes regardless of `output_limit`, so a status
or wait after the fact may report a shorter, marked-truncated text than
the original return. `usage` reports the run's token totals and cost when
the provider reported them. A timed-out run keeps only the history from
before that run, so a resume never replays a dangling tool call. Argument
problems — a missing `prompt`, an empty or unknown `id`, a bad override, a
value with the wrong type — resolve with `ok: false` and `status: "failed"`
instead of throwing.

Subagents run with the session's shell policy and truncation config; they
cannot spawn further subagents. They write transcripts unless the session
does not persist (`--no-session`, print mode). Each session keeps at most
64 subagent entries; past that, spawning evicts a finished entry, and it
fails while 64 runs are still active.

## lofi.subagentSpawn({ prompt, model?, thinking?, system?, timeout_ms?, token_budget? })

Start a background subagent and return immediately. The run survives the
end of this exec; unlike `lofi.subagent`, cancelling the exec does not
cancel the run. Take the result later with `lofi.subagentWait`.

**Parameters:** same as `lofi.subagent`, except `id` (resume) and
`output_limit` are not accepted; resume stays a foreground operation.

**Returns:** `{ ok, id, status, session_id? }`. `status` is `running` on
success, `failed` with an `error` on rejection (concurrency cap, bad
override, unknown argument problem). Keep the `id`: it is the handle for
`subagentStatus`, `subagentWait`, `subagentRead`, `subagentKill`, and
`subagentNotify`.

## lofi.subagentStatus({ id })

Report one subagent run's state.

**Parameters:**
- `id` (string, required) — the run id from `subagentSpawn` or `subagent`.

**Returns:** `{ ok, id, status, session_id?, usage?, error? }`. `status` is
`running`, `completed`, `failed`, `cancelled`, or `timed_out`; `error` is
set for `failed`. Unknown ids resolve with `ok: false`.

## lofi.subagentList({ state?, limit?, cursor? })

List this session's subagent runs, oldest first.

**Parameters:**
- `state` (string, optional) — `running` or `terminal`; omit for all.
- `limit` (number, optional) — page size, default 20, maximum 100.
- `cursor` (string, optional) — opaque page cursor from a previous call.

**Returns:** `{ ok, entries: [{ id, status, session_id? }], cursor? }`.
`cursor` is present while more entries remain.

## lofi.subagentWait({ id, timeout_ms? })

Wait for a background subagent to reach a terminal state. A wait timeout
never cancels the run: the run keeps going and can be awaited again.

**Parameters:**
- `id` (string, required) — the run id.
- `timeout_ms` (number, optional) — wait limit, default 60_000. Keep it
  below the exec's own timeout.

**Returns:** the same shape as `lofi.subagent` once the run is terminal;
`{ ok, id, status: "running" }` when the wait timed out.

## lofi.subagentRead({ id, cursor?, limit?, types? })

Read a run's events incrementally. Events are sequence-numbered; the cursor
is the next sequence number.

**Parameters:**
- `id` (string, required) — the run id.
- `cursor` (number, optional) — sequence number to start from, default 0.
- `limit` (number, optional) — page size, default 50, maximum 200.
- `types` (array of string, optional) — event types to include; the cursor
  still advances over filtered-out events.

**Returns:** `{ ok, id, events: [{ seq, type, ... }], cursor, dropped,
done }`. Event `type` is one of `prompt`, `text`, `tool_start`, `tool_end`,
`usage`, `state`. `dropped` counts events that fell out of the retained
buffer before `cursor` (the buffer is bounded); `done` is true when the run
is terminal and all retained events are read. The final `state` event
carries the terminal status. The buffer lives until the run's entry is
evicted; afterwards use the transcript (`session_id`) instead.

## lofi.subagentKill({ id, reason? })

Cancel a running subagent and the jobs it spawned. Idempotent: killing a
terminal run returns its current status.

**Parameters:**
- `id` (string, required) — the run id.
- `reason` (string, optional) — recorded in the run's terminal `state`
  event.

**Returns:** `{ ok, id, status }`.

## lofi.subagentNotify({ id, interval_ms?, enabled? })

Configure host notices for a run. Runs started with `subagentSpawn`
publish a terminal notice (status and short summary) by default;
foreground `subagent` runs return their outcome directly and publish
nothing. This toggles the notice or adds periodic progress notices.

**Parameters:**
- `id` (string, required) — the run id.
- `interval_ms` (number, optional) — periodic notice interval, minimum
  5_000.
- `enabled` (boolean, optional) — set `false` to silence the terminal
  and progress notices for this run. A message the run sends to `parent`
  still wakes the session when the parent inbox was empty, because asking
  for an answer is the point of the message.

**Returns:** `{ ok, id }`.

## lofi.subagentMailboxSend({ to, body, reply_to?, key? })

Send one message to the parent or to a run. A subagent may address only
`parent`; the session addresses a live run id. The sender is taken from
the caller's capability, never from the arguments, so a subagent cannot
address a sibling.

**Parameters:**
- `to` (string, required) — `parent` from a subagent, or a live run id
  from the session.
- `body` (string, required) — the message, at most 8 KiB.
- `reply_to` (number, optional) — the `seq` of the message being answered.
- `key` (string, optional) — idempotency key. A retry with the same key
  returns the original `seq` with `duplicate: true`. The same key with
  different content is a conflict.

**Returns:** `{ ok, seq, to }`, or `{ ok: false, status, error }` with
`status` one of `failed`, `forbidden`, `sender_ended`,
`unknown_recipient`, `recipient_ended`, `full`, `conflict`,
`too_large`, `persist_failed`. `sender_ended` means the caller's own run
has ended, so it may no longer write. A repeat of `key` returns the first
`seq` with `duplicate: true`, even when the recipient has ended since.

A message to `parent` wakes the session: it stores one notice prompt
naming the oldest sender, and the session reads the mail with
`lofi.subagentMailboxReceive`. The wake fires when the parent inbox goes
from empty to non-empty, so ten messages from three children cost one
wake. Only a `receive` that empties the parent inbox arms the next wake:
`lofi.subagentMailboxList`, a partial `receive`, and a turn the user
cancels all leave the inbox nonempty, and then no new message wakes the
session until the inbox is drained. The notice carries no body. A wake
never interrupts a running turn: the session finishes its work first, then
starts the turn unless the inbox is already empty. A session that stays in
`lofi.subagentMailboxReceive` needs no wake, because the wait takes the
message directly. Mail that a restart replays into the parent inbox wakes
once, after the session's next turn, unless that turn reads the inbox.

The wake reaches a host that subscribes to notices, which the TUI does.
Print mode does not subscribe, so it reads the inbox when it next runs.

## lofi.subagentMailboxReceive({ limit?, wait_ms? })

Take pending messages from the caller's inbox, oldest first. Messages are
numbered by one counter for the whole mailbox, so the parent sees a single
order across all children.

**Parameters:**
- `limit` (number, optional) — maximum messages, default 20, maximum 100.
- `wait_ms` (number, optional) — wait for the first message. Default 0
  returns at once; the maximum is 60_000. A wait never reads past the
  exec timeout and never blocks the sending side.

**Returns:** `{ ok, messages }` where each message is
`{ seq, from, to, reply_to, body }`; `{ ok, messages: [], empty: true }`
when nothing is pending, with `timed_out: true` when the wait expired;
`{ ok, messages: [], closed: true }` when the caller's run has ended or
the session that owned it has been replaced.

A message that arrives for a run which then ends is never delivered to
that run. It stays visible to the parent, and a later resume of the same
id delivers it to the new run. The read is recorded when the caller takes
the messages: a crash before the hand-off delivers them again on the
resume, while a run that ends after it does not get them back.

## lofi.subagentMailboxList({ agent_id? })

List pending messages. A subagent sees only its own inbox. The session
sees its own inbox and, with `agent_id`, one run's inbox; without
`agent_id` it also gets a per-address count.

**Parameters:**
- `agent_id` (string, optional) — run id to inspect. A subagent may pass
  only its own id.

**Returns:** `{ ok, agent_id, messages }`, or for the session without
`agent_id`, `{ ok, messages, inboxes: [{ agent_id, pending }] }`. A
message a run never read carries `undelivered: <reason>`, where the
reason is the run's final status.

## lofi.models()

List the models available for `lofi.subagent` overrides. Like every `lofi.*`
call it returns a promise: `await lofi.models()`.

**Returns:** `{ models: [{ model, name, thinking_levels, service_tiers }] }`,
where `model` is the qualified query (`provider/id`). Without a host
registry, returns `{ ok: false, status: "unavailable" }`.

## lofi.skills(search?)

List available skills, optionally filtered.

**Parameters:**
- `search` (string, optional) — case-insensitive substring matched against each
  skill's name and description. Omit to list all skills.

**Returns:** `{ ok, skills }` — each entry is
`{ name, description, source, path }`. `source` is `"global"` (from
`<config_dir>/skills/`) or `"workspace"` (from `<root>/.lofi/skills/`). `path`
is the skill directory's absolute filesystem path (a read root).

## lofi.skill(name)

Read a single skill's `SKILL.md`.

**Parameters:**
- `name` (string, required) — the skill name (may contain `/` as namespace
  separator, e.g. `git-workflow/rebase`).

**Returns:** `{ ok, name, source, file, path, content }`. When both sources
define the same name, the workspace version wins.

Skills are directories containing a `SKILL.md` file. They may carry companion
files alongside `SKILL.md`. Use `lofi.skills()` to discover, then
`lofi.skill(name)` to read. To access companion files, pass the absolute
`path` to the regular file tools (e.g. `lofi.read(path + "/examples/foo.md")`,
`lofi.grep("pattern", path)`).

## lofi.docs(name?)

Get the API reference index or a specific entry.

**Parameters:**
- `name` (string, optional) — an entry name like `lofi.read`. Omit to get the
  full index.

**Returns (no arg):** `{ ok, entries: [{ name, summary }] }`.
**Returns (with name):** `{ ok, name, content }` — the full markdown text for
that entry, or `{ ok: false, error }` if not found.

## lofi.docsSearch(query)

Search the API reference by keyword.

**Parameters:**
- `query` (string, required) — search terms. Matches entry names and bodies;
  name matches score higher.

**Returns:** `{ ok, results: [{ name, score, excerpt }] }` — sorted by score
descending, limited to the top 10.

## Truncated results and filtering

Two kinds of "too much" are handled differently:

- **Content tools page.** `read` returns a window of a file, head-truncated to the configured `[truncate]` cap, with a `truncated` flag plus `total_lines` and `start_line`. Before you `.filter`/`.map`/`.includes`/`.split`
  their `content`, check `.truncated`: if `true`, more lines remain — page with
  a higher `offset` until `truncated` is `false` before processing the whole
  file.

- **List tools throw.** `ls`, `find`, and `grep` never return a partial set —
  they have no `truncated` flag and no `limit`/`max` parameter. If a result
  would exceed a safety ceiling, the call throws with a "narrow the ..."
  message. Treat that as the signal to narrow and re-run. Do not catch it and
  filter whatever you got back — you got nothing.

Prefer narrowing the query server-side over filtering client-side when the
result might be large.

## Top-level await and return

Code runs as `(async () => { ... })()`. You may `await` anything at the top
level and `return` the final value. Example:

```ts
const files = await lofi.ls("src");
const a = await lofi.read("src/a.ts");
return { files: files.entries, len: a.content.length };
```

## print and logs

`print(...)` appends to a log buffer returned in the tool result's `logs`
field. Use `print` for progress/scratch output; `return` the final,
decision-relevant value. Both fields are sent back to you.

Await every foreground `lofi` call. The sandbox drains omitted promises
before it exits and reports rejected calls or results with `ok: false` in the
parent result's `errors` field. Such failures make the parent result fail.
Use `jobSpawn` when work must continue in the background.

## Compacted sessions

When a session grows long, lofi folds older history into a structured summary
and injects it as a single user message at the head of the kept tail. The
summary begins with a preamble ("This summary captures work done before the
most recent messages in this session..."), followed by tagged sections and a
compressed per-turn transcript. Treat the summary as accurate context and
continue from it. The full transcript remains on disk, so `/tree` can still
roll back past the compaction point.