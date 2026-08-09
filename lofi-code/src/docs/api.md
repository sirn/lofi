# lofi API reference

The agent's single tool is `exec`. Inside `exec` code, call async functions on
the global `lofi` object. Use `await`. All file paths are resolved against the
workspace root; paths that escape the root are rejected.

## lofi.read(path, opts?)

Read a file as UTF-8 with optional line range, or attach an image.

**Parameters:**
- `path` (string, required) — file path relative to workspace root.
- `opts` (object, optional) — `{ offset?, limit? }`. `offset` is a 1-indexed
  line to start from (default 1); `limit` caps the number of lines returned
  (default 2000).

**Returns (text):** `{ ok, content, start_line, total_lines, truncated }`.
`content` is the requested lines (head-truncated to 2000 lines / 50 KB).
`start_line` is the 1-indexed first line returned. `total_lines` is the file's
line count. `truncated` is true when more lines remain below either cap. Page
large files with a higher `offset`.

**Returns (image):** for a path with an image extension (`.png`, `.jpg`,
`.jpeg`, `.gif`, `.webp`, `.bmp`), returns `{ ok, type: "image", media_type,
data_b64 }` instead. The host decodes `data_b64` and attaches the image to the
conversation as vision input, so reading an image file is how you look at it.
The image path ignores `offset`/`limit`.

## lofi.ls(dir?)

List entries under a directory.

**Parameters:**
- `dir` (string, optional) — directory relative to workspace root. Empty or
  `.` means the root.

**Returns:** `{ ok, entries }` — a sorted array of relative paths. The result
is always complete: if the directory has more than 50,000 entries the call
throws (narrow with a more specific `dir`).

## lofi.find(glob, dir?, filtered?)

Recursive glob match.

**Parameters:**
- `glob` (string, required) — glob pattern.
- `dir` (string, optional) — directory to search in (default root).
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
- `path` (string, optional) — file or directory to search (default root).

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

## lofi.bash({ cmd, timeoutMs? })

Run a shell command with `sh -c`, cwd pinned to the workspace root. Stdout and
stderr are merged.

**Parameters:**
- `cmd` (string, required) — the shell command.
- `timeoutMs` (number, optional) — timeout in milliseconds (default 120000).

**Returns:** `{ ok, output, code, command, directory, signal, duration_ms,
status }`. `code` is the exit status (null on signal/timeout); `signal` is the
Unix signal number (null unless killed by a signal); `duration_ms` is wall
time; `status` is `"exited"`, `"signaled"`, or `"timeout"`. Output is
tail-truncated to 20 lines / 4 KB (keeping the end where errors land); when
truncated, the full output is saved to a file under `lofi.tmp_dir` and the
notice names it — page through it with `lofi.read(path)`. The child env is
stripped to a minimal baseline by default; env vars the user approved are
present but their values are replaced with `[redacted]` in the output.

## lofi.job_spawn({ cmd, timeoutMs? })

Start a background shell command without blocking the current turn. The
command runs `sh -c` from the workspace root in its own process group with
the same shell policy, stripped environment, and output redaction as
`lofi.bash`. Returns immediately.

**Parameters:**
- `cmd` (string, required) — the shell command.
- `timeoutMs` (number, optional) — timeout in milliseconds (default 600000).
  On timeout the whole process group is killed and the job ends as
  `"timed_out"`.

**Returns:** `{ ok, id, state, command, directory, pid, timeout_ms,
log_path }`. `state` starts as `"running"`. `log_path` names the merged
stdout/stderr log under `lofi.tmp_dir`; it is a read root, so `lofi.read` on
it also works. Output is capped at 64 MiB; a job that exceeds the cap is
killed and reported as failed.

## lofi.job_status({ id })

Current state, timestamps, exit status, and limits for a job.

**Returns:** `{ ok, id, state, command, directory, pid, exit_code, signal,
duration_ms, timeout_ms, log_path, notify }`. `state` is `"running"`,
`"completed"`, `"failed"`, `"cancelled"`, or `"timed_out"`. Unknown ids
return `{ ok: false, error }`.

## lofi.job_read({ id, cursor?, limit? })

Incremental read of a job's merged stdout/stderr log.

**Parameters:**
- `id` (string, required) — job id from `job_spawn`.
- `cursor` (number, optional) — byte offset to resume from (default 0).
- `limit` (number, optional) — max bytes to return (default and max 65536).

**Returns:** `{ ok, id, state, cursor, total_bytes, output, done }`.
`cursor` is the next offset to pass. `output` is redacted. `done` is true
once the job is terminal.

## lofi.job_wait({ id, timeout_ms? })

Bounded wait for a job to reach a terminal state. Waiting never cancels the
job; it returns the still-running status when `timeout_ms` elapses.

**Returns:** the same shape as `job_status`.

## lofi.job_kill({ id, reason? })

Cancel a job: SIGKILL its entire process group (children and grandchildren)
and mark it `"cancelled"`. Idempotent — killing an already-terminal job is
a no-op that returns its current status.

**Returns:** the same shape as `job_status`, plus `reason`.

## lofi.job_notify({ id, enabled? })

Toggle the completion notification. When enabled (the default), a terminal
transition queues a one-line notice that the agent injects at the next round
boundary and the UI surfaces live. Set `enabled: false` after collecting a
result with `job_wait` to suppress a redundant notice. `interval_ms` is
accepted for forward compatibility; periodic progress pings are not emitted.

**Returns:** `{ ok, id, notify }`.

## lofi.tmp_dir

A string property: the absolute path to the per-session tmp directory backing
`lofi.bash` full-output logs. This directory is a read root — `lofi.read`,
`lofi.ls`, `lofi.find`, and `lofi.grep` can access files under it.


## lofi.recall({ query?, scope?, page?, expand? })

Search the full session transcript (including messages a compaction folded
away) and return rendered matches as text.

**Parameters:**
- `query` (string, optional) — a regex or multi-word BM25 query. Omit to
  browse the most recent entries.
- `scope` (string, optional) — `"lineage"` (default, active branch),
  `"all"` (whole session), `"compaction:N"` / `"compaction:latest"` (within
  one compaction's summarized range).
- `page` (number, optional) — 1-based page number.
- `expand` (array, optional) — indices of entries to return with full
  untruncated content.

**Returns:** `{ text, status }`.

## lofi.result(eventId)

Recover the original, full content of a tool result or tool-call that
compaction elided.

**Parameters:**
- `eventId` (string, required) — the event id from a compaction stub.

**Returns:** `string` — the raw content (a tool result's output text, or a
tool call's input JSON). Cheap and on-demand.

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

- **Content tools page.** `read` returns a window of a file
  (head-truncated to 2000 lines / 50 KB) carrying a `truncated` flag plus
  `total_lines`/`start_line`. Before you `.filter`/`.map`/`.includes`/`.split`
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

## Compacted sessions

When a session grows long, lofi folds older history into a structured summary
and injects it as a single user message at the head of the kept tail. The
summary begins with a preamble ("This summary captures work done before the
most recent messages in this session..."), followed by tagged sections and a
compressed per-turn transcript. Treat the summary as accurate context and
continue from it. The full transcript remains on disk, so `/tree` can still
roll back past the compaction point.