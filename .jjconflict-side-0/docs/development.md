# Development

## End-to-end tests

The `lofi` crate has deterministic end-to-end tests in `lofi/e2e/`. They run the real binary in a pseudo-terminal or print mode and use a local mock LLM server. The suite covers CLI commands, OpenAI Completions and Responses streaming, thinking and usage, retries, errors, tools, permissions, cancellation, direct shell input, model controls, sessions, resume, compaction, recall, branches, and background jobs. The tests do not need API keys and do not make external requests. Run them with:

```sh
just e2e
```

Run all format, lint, and test checks with `just check`.

### Bugs found by the E2E suite

The first full process-level pass found and fixed these bugs:

| Area | Previous behavior | Fix and regression coverage |
| --- | --- | --- |
| New sessions | `/new` cleared the UI cursor but left the session sink attached to the old transcript. The next prompt could append to the old session instead of creating a new file. | Detach both cursors. Session lifecycle E2E tests assert separate histories and correct `--continue` and `--resume` selection. |
| Recall command | `/recall <query>` did not match the exact `/recall` command arm and was reported as an unknown command. | Accept the command with an argument. A PTY test runs a query and checks its results. |
| Direct shell cancellation | `Ctrl+C` set a cancellation flag, but a running `!` command did not observe it. The command and its process group could survive cancellation or TUI shutdown. | Pass the cancellation token into the shell runner and terminate the process group before settling the event. PTY tests check cancellation and shutdown with live commands. |
| Provider retries | Premature response-body EOF errors and some protocol terminal-event errors were not retried. Plain numeric status matching could also mistake digits in a URL or port for an HTTP status. | Retry typed HTTP transport errors, recognize all provider premature-end messages, and require status-code word boundaries. E2E tests truncate streams for all four provider protocols and assert a successful retry. |
| Skill metadata | Skill discovery could use a frontmatter field such as `name:` as the description instead of reading `description:`. Compaction used a separate parser, so the two views could disagree. | Use one metadata parser for skill indexing and compaction. E2E tests assert lazy indexing, descriptions, namespaced skills, and workspace overrides. |
| Popup stacking | A policy confirmation could render over `/tree` while keys still went to the tree picker. Autocomplete could also paint over centered modals because render and input orders differed. | Use the reverse render order for input dispatch and render autocomplete below centered modals. Unit and PTY tests assert that the top policy confirmation handles input before `/tree`. |
| Job cancellation | A queued background-job completion could disappear when ESC cancelled the foreground command. A new or resumed session could also inherit old processes and notices. | Cancel only the foreground run, keep queued notices for the next turn, and scope job registries to the active session generation. PTY tests cover cancellation, `/new`, `/resume`, and stale notices. |
| Partial transcripts | A process killed between an event write and its newline made resume reject the whole transcript. | Ignore only a malformed final line and keep the committed prefix resumable. Process tests cover partial tails, absent cursor records, unsupported versions, and state permission repair. |
| Custom shell wrappers | A configured wrapper's allowed inner command could still be reported as an unmatched outer command. | Do not let an extracted wrapper's own name veto the inner command's policy result. E2E tests cover match modes, wrappers, redirects, heredocs, policy modes, and YOLO. |

## Session transcripts

`lofi` persists each interactive session as a JSON Lines transcript under `$XDG_STATE_HOME/lofi/sessions/<workspace-name>-<path-hash>/`, with one `.jsonl` file per session. The hash isolates workspaces whose readable names collide; legacy slug-only directories remain readable. The first line is a `meta` header containing `version`, `created`, `cwd`, and the raw `model` identity. Every subsequent line is a session event. `--no-session` disables persistence, `--continue` resumes the most recently active session for the workspace, and `--resume <id>` resumes a specific one.

Non-metadata events form a **tree** through `id` and `parent_id`. Each new event points to its parent, and branching appends a new child without removing existing siblings. The selected logical head is stored separately in append-only `cursor` records; physical end-of-file order is not the authoritative active branch. Resuming walks from that selected head to the root and rebuilds history from the resulting active path, so sibling branches do not enter the model's context.

The current event kinds are:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `message` | `role`, `blocks` | A user, assistant, system, or tool-result message. |
| `tool_timing` | `tool_call_id`, `elapsed_ms` | Wall-clock duration of a completed tool call, so the `took Ns` marker survives resume. |
| `thinking_timing` | `elapsed_ms` | Wall-clock duration of a completed thinking block. |
| `native_tool` | `parent`, `call_id`, `name`, `args`, `result`, `is_error` | A nested `lofi.<tool>` call inside an `exec` block. |
| `turn_end` | `model`, `elapsed_ms`, `cost`, `usage` | A completed turn, rendered as `◇ Done in Ns with <model>`. |
| `turn_failed` | `model`, `elapsed_ms`, `error`, `cost`, `usage` | A failed or cancelled turn, rendered as `◇ Failed in Ns with <model>`. |
| `cursor` | `leaf_id` | Metadata selecting the transcript's logical head; it is not part of the conversation tree. |
| `compaction` | `summary`, boundary/range ids, checkpoint flags, and message counts | A durable compaction checkpoint that replaces an older prefix with a summary for model-history replay. |

The recorder checkpoints completed provider/tool rounds during a turn, then appends the remaining events and a terminal marker when the turn settles. A hard-context-pressure stop writes the completed partial rounds but no terminal marker because the run is compacted and continued.

Failed and cancelled turns with recordable content are **recorded, not dropped**. Their partial messages, timings, and a `turn_failed` marker remain on the active path, so the attempt stays visible after resume and its accumulated cost and usage remain in the totals. Model-history reconstruction walks the path from newest to oldest; a `turn_failed` marker suppresses messages until the preceding `turn_end`, so partial failed content is not sent back to the model.

The current transcript format is version 3. Versions 1 through 3 can be loaded. Version 1 stored legacy messages without tree ids; loading assigns stable in-memory ids and chains them linearly. Files without durable cursor metadata fall back to the last non-cursor event when first opened.

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

- `lofi.read(path, opts?)`
- `lofi.ls(dir?)`
- `lofi.find(glob, dir?)`
- `lofi.grep(pattern, path?)`
- `lofi.write({ path, text })`
- `lofi.edit({ path, old, new })`
- `lofi.bash({ cmd, timeoutMs? })`
- `lofi.skills()` and `lofi.skill(name)`
- `lofi.docs(name?)` and `lofi.docsSearch(query)`
- `lofi.recall(request)` and `lofi.result(eventId)` (they report unavailability without a persisted session)
- `lofi.tmp_dir`, the per-session temporary directory path

The precise arguments, structured return values, paging limits, and safety caps are documented in the embedded API reference (`lofi --docs`) and `lofi-code/src/docs/api.md`.

Read-only file tools accept workspace-relative paths and selected absolute paths under explicitly registered read roots, such as the session temporary directory and skill directories. Write and edit operations remain confined to the workspace root and reject path escapes and symlink leaves. `bash` runs through `sh -c` with its working directory pinned to the workspace root; shell-policy, environment-stripping, redaction, and timeout rules are applied by the host. Its visible output keeps the last 20 lines or 4 KiB, whichever limit is reached first, and links to a pageable temporary log when truncated.

`print(...)` appends to a bounded log buffer instead of writing to host stdout. The async IIFE's returned value and the log buffer become the `exec` result sent back to the model. QuickJS heap, stack, synchronous CPU time, converted values, and printed logs are bounded; awaited native tools use their own limits and do not consume the guest's synchronous CPU budget.

## Lint strictness

Every workspace crate inherits the root lint configuration. It denies `clippy::unwrap_used`, `clippy::expect_used`, `clippy::todo`, `clippy::dbg_macro`, and `clippy::print_stdout`. Production code generally propagates or handles errors explicitly and writes CLI output through `std::io::Write` rather than `println!`; the interactive UI renders through ratatui. Narrow, documented exceptions exist where an invariant or a compile-time constant makes `expect` appropriate. Test modules commonly allow `unwrap_used` and `expect_used` to keep assertions readable.
