# Development

## End-to-end tests

See [End-to-end testing](e2e.md) for the current coverage map, test architecture, focused commands, and contribution guidance.

Run the process-level suite with `just e2e`. Run all format, lint, and workspace tests with `just check`.

## Session transcripts

`lofi` persists each interactive session as a JSON Lines transcript under `$XDG_STATE_HOME/lofi/sessions/<workspace-name>-<path-hash>/`, with one `.jsonl` file per session. The hash isolates workspaces whose readable names collide; legacy slug-only directories remain readable. The first line is a `meta` header containing `version`, `created`, `cwd`, and the raw `model` identity. Every subsequent line is a session event. `--no-session` disables persistence, `--continue` resumes the most recently active session for the workspace, and `--resume <id>` resumes a specific one.

Non-metadata events form a **tree** through `id` and `parent_id`. Each new event points to its parent, and branching appends a new child without removing existing siblings. The selected logical head is stored separately in append-only `cursor` records; physical end-of-file order is not the authoritative active branch. Resuming walks from that selected head to the root and rebuilds history from the resulting active path, so sibling branches do not enter the model's context.

The current event kinds are:

| `type` | Main fields | Meaning |
| --- | --- | --- |
| `message` | `role`, `blocks`, `kind` | A system, user, assistant, or tool-result message. `kind` distinguishes typed prompts from injected notices. |
| `user_shell` | `command`, `output`, exit data, duration, truncation, cancellation, context flag | A direct `!` or `!!` command and its settled result. |
| `tool_timing` | `tool_call_id`, `elapsed_ms` | Wall-clock duration of a completed provider tool call. |
| `thinking_timing` | `elapsed_ms` | Wall-clock duration of one completed thinking block. |
| `native_tool` | `parent`, `call_id`, `name`, `args`, `result`, `is_error` | A nested `lofi.<tool>` call inside an `exec` block. |
| `round_discarded` | `detail` | A streamed round kept for display but removed from later model context after loop recovery or a retry re-roll. |
| `turn_end` | `model`, `elapsed_ms`, `cost`, `usage`, `stop_reason` | A completed turn. |
| `turn_failed` | `model`, `elapsed_ms`, `error`, `cost`, `usage` | A failed turn whose partial output remains visible but is excluded from later model context. |
| `turn_cancelled` | `model`, `elapsed_ms`, `cost`, `usage` | A user-cancelled turn whose partial output remains visible and available to later turns. |
| `cursor` | `leaf_id` | Metadata that selects the transcript's logical head. It is not part of the conversation tree. |
| `compaction` | Summary, range ids, checkpoint flag, and message counts | A durable checkpoint that replaces an older model-history prefix with a summary and retained tail. |
| `job_started` / `job_finished` | `job_id` | Invisible lineage markers used to reconcile session-owned background jobs. |

The recorder checkpoints each completed provider and tool round. It then appends the remaining events and a terminal marker when the turn settles. A hard-context-pressure stop writes the available turn data without a terminal marker because compaction continues the same turn.

Failed, cancelled, and discarded rounds are all durable, but they have different replay rules. Failed and discarded output stays visible but is excluded from later model context. Cancelled output stays visible and remains in context so the user can continue from the interrupted work. Cost and usage stay attached to each terminal marker.

The current transcript format is version 1. A different version is rejected. Unknown event kinds inside a version-1 transcript remain loadable and are ignored by consumers. Files without cursor metadata fall back to the last non-cursor event when first opened. A malformed final line is ignored so a committed prefix remains resumable; malformed content earlier in the file is an error.

## Code mode

The only tool advertised to the LLM is `exec`:

```jsonc
{
  "code": "<TypeScript source>",
  "strings": { "key": "value" }, // optional; exposed as `lofi_strings`
  "display": { ... }              // optional UI metadata; ignored by the runtime
}
```

The `code` is parsed as TypeScript, stripped of types with swc, wrapped in an async IIFE so top-level `await` and `return` work, and run in a fresh embedded QuickJS runtime. The sandbox exposes a global `lofi` object. Its current bindings are:

- File and search: `read`, `ls`, `find`, `grep`, `write`, `edit`, and `patch`.
- Foreground shell: `bash`.
- Background jobs: `jobSpawn`, `jobStatus`, `jobList`, `jobRead`, `jobScreen`, `jobWait`, `jobKill`, `jobNotify`, `jobType`, and `jobKeyPress`.
- Session recovery: `recall` and `result`. They report unavailability without a persisted session.
- Skills: `skills` and `skill`.
- Embedded reference: `docs` and `docsSearch`.
- `tmp_dir`, the per-session temporary directory path.

The precise arguments, structured return values, paging limits, and safety caps are documented in the embedded API reference (`lofi --docs`) and `lofi-code/src/docs/api.md`.

Read-only file tools accept workspace-relative paths and selected absolute paths under explicitly registered read roots, such as the session temporary directory and skill directories. Write, edit, and patch operations remain confined to the workspace root and reject path escapes and symlink leaves. `bash` and background jobs run through `sh -c` with their working directory pinned to the workspace root. The host applies shell policy, environment stripping, output redaction, cancellation, and process-group cleanup. `bash` uses the configured `[truncate]` limits and links to a pageable temporary log when its visible tail is truncated. Background job logs are read incrementally with `jobRead`.

`print(...)` appends to a bounded log buffer instead of writing to host stdout. The async IIFE's returned value and the log buffer become the `exec` result sent back to the model. QuickJS heap, stack, synchronous CPU time, converted values, and printed logs are bounded; awaited native tools use their own limits and do not consume the guest's synchronous CPU budget.

## Lint strictness

Every workspace crate inherits the root lint configuration. It denies `clippy::unwrap_used`, `clippy::expect_used`, `clippy::todo`, `clippy::dbg_macro`, and `clippy::print_stdout`. Production code generally propagates or handles errors explicitly and writes CLI output through `std::io::Write` rather than `println!`; the interactive UI renders through ratatui. Narrow, documented exceptions exist where an invariant or a compile-time constant makes `expect` appropriate. Test modules commonly allow `unwrap_used` and `expect_used` to keep assertions readable.
