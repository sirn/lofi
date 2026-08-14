# Lofi

Lofi is a minimal coding-agent harness written in Rust.

## Platform support

The initial release targets Unix-like systems with POSIX shell and process semantics. Linux is the primary tested platform; macOS and other Unix targets are best effort. Windows is not currently supported.

## Building

```sh
cargo build --release
target/release/lofi
```

## End-to-end test coverage

The deterministic suite in [`lofi/e2e/`](lofi/e2e/) runs the real `lofi` binary in a pseudo-terminal or in print mode. It uses a local mock model server. It does not use API keys or make external requests.

In this matrix, **Full** means that all principal public workflows and entry points in the area have process-level tests. **Partial** means that the main workflow has a process-level test, but the listed branches do not. **Missing** means that coverage is limited to unit or crate-level integration tests, or that no automated coverage exists. Lower-level tests are not counted as E2E here.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| Session lifecycle | **Full** | Lazy creation, `/new`, `--continue`, explicit and picker resume, latest-session selection, workspace isolation, model restore and override, no-model transcript access, and restart recovery after failed, cancelled, or interrupted turns. | None known for the supported public lifecycle. |
| Session branches and job ownership | **Full** | Rollback, active-branch persistence, sibling exclusion, old-branch retention, job ownership across later calls, off-lineage job cleanup, and stale jobs after process restart. | None known for the supported public lifecycle. |
| System prompt assembly | **Partial** | Global and project `AGENTS.md`, nested project instructions in outermost-first order, skill metadata injection without eager body loading, de-duplication, and stable prompt replay after resume. | Add workspace skill override, namespaced skills, invalid and oversized skill files, scan limits, empty files, and the other project-root boundary markers. |
| Thinking-level controls | **Full** | `off`, `low`, `medium`, `high`, and `xhigh`; CLI selection; TUI picker selection; request propagation; resume restore; and explicit override. | None known for the supported levels and controls. |
| Background job API and UI | **Partial** | Spawn, status, paged read, wait, notifications, timeout, kill, missing ids, completion notices, modal list and logs, and explicit process-group cleanup. | Add graceful TUI shutdown with live jobs, periodic and changed-only notice timing, multiple simultaneous jobs in the modal, and log-window truncation. |
| Native file, search, docs, and skill APIs | **Partial** | `read`, `ls`, `find`, `grep`, `write`, `edit`, `patch`, `bash`, `skills`, `skill`, `docs`, `docsSearch`, and `tmp_dir`; successful round trips, contained errors, filtering, paging, and truncation recovery. | Add workspace and namespaced skills, missing and invalid API entries, all option forms, binary and invalid UTF-8 files, symlink and traversal permutations, write races, and each walk and result limit. |
| CLI entry points | **Partial** | Print mode, model selection, session listing, invalid resume ids, docs, docs search, policy explanation, model listing, provider failures, and no-TTY operation. | Add process tests for `--help`, `--version`, conflicting flag precedence, malformed configuration, and signal exit status. |
| TUI slash commands and modals | **Partial** | Autocomplete, help, session, theme, model, thinking, jobs, debug, verbose, recall, clear, compact, new, resume, tree, quit, exit, and unknown commands. | Add full keyboard navigation, scrolling, copy, cancellation, and narrow-terminal tests for each modal. |
| TUI input and navigation | **Partial** | Prompt submission, queued prompts, bracketed multiline paste, input clearing, quit, cancellation, and raw-Markdown copy through OSC 52. | Add process tests for multiline editing, history recall keys, all Navigate/Select motions, resize reflow, Unicode width, focus events, and malformed or split terminal input sequences. |
| Transcript rendering | **Partial** | Streamed text and thinking, tool blocks, status transitions, selected raw Markdown, and resumed transcript visibility are exercised through the TUI. | Markdown structures, links, tables, code blocks, wrapping, themes, viewport anchoring, and resize behavior remain unit-only. |
| OpenAI Chat Completions | **Partial** | Text and usage streaming, thinking-level request fields, fragmented tool arguments, parallel tools, tool results, errors, retries, and images through a vision model. | Add provider-specific E2E for reasoning-detail signatures, in-stream error objects, unexpected EOF, image tool results, custom headers, and no-auth requests. |
| OpenAI Responses | **Partial** | Text, reasoning summaries, usage, default summary request, thinking off, encrypted reasoning replay, tools, truncated-stream retry, and resume replay. | Add image input and tool-result E2E, multiple reasoning summary parts, in-stream failed events, unexpected EOF, custom headers, and no-auth requests. |
| Anthropic Messages | **Partial** | Text, tools, tool results, adaptive thinking, signed thinking replay, usage, and cache breakpoints. | Add image E2E, parallel and malformed tool calls, refusal or error events, unexpected EOF, custom headers, and no-auth requests. |
| Google Generative AI | **Partial** | Text, function calls and responses, thinking budgets, thought streaming, signatures, signature replay, usage, and native endpoint routing. | Add image E2E, Gemini 3 thinking-level mappings, parallel and malformed function calls, safety and error responses, unexpected EOF, custom headers, and no-auth requests. |
| Agent failure and recovery | **Partial** | Retry after a transient HTTP failure, no retry after authentication failure, truncated-stream retry, tool failure recovery, cancellation, and durable prompts before provider response. | Add retry exhaustion, configured delay and limit behavior, stream timeout, cancellation during retry delay, receiver loss, and failure after a completed tool round. |
| Provider tool protocol | **Partial** | Tool execution and result replay for all four APIs, fragmented and parallel OpenAI Chat calls, cancellation closure after restart, and a valid tool cycle after compaction. | Add parallel and interleaved calls for Responses, Anthropic, and Google; malformed arguments; duplicate ids; orphan results; and provider-specific image results. |
| Direct shell input | **Partial** | `!` and `!!` context control, transcript persistence, restart replay, cancellation, and process-group cleanup. | Add non-zero exit, signal death, large-output paging, environment redaction, and shutdown while output is still streaming. |
| Compaction | **Partial** | Manual, automatic soft-threshold, hard-pressure continuation, resume, branch selection, and preservation of the latest tool cycle. | Add tiered retention settings, repeated hard-compaction guard, compact-all, image removal, summary budget limits, and failure during compaction. |
| Recall and result recovery | **Partial** | TUI query recall, native recall by query, and exact tool-result recovery by event id. | Add all scope and pagination modes, compacted and off-branch content, missing ids, truncated values, and behavior with `--no-session`. |
| Model discovery and selection | **Partial** | Static models, remote field and API mapping, online discovery, offline cache fallback, explicit models, picker changes, missing models, and resume overrides. | Add authenticated discovery, cache expiry and corruption, static/discovered merge precedence, default provider and model selection, custom endpoint paths, and ambiguous model queries. |
| Configuration loading | **Partial** | Bash environment files, secret redaction, automatic policy approval, model discovery, truncation limits, no-model mode, and selected compaction settings. | Add environment and shell value resolution, `LOFI_CONFIG` and XDG path precedence, invalid TOML, custom headers, `no_auth`, image limits, retry settings, UI theme config, and all default-precedence rules. |
| Shell policy | **Partial** | Interactive allow and deny, unrestricted fixture execution, automatic model approval, and CLI policy explanation. | Add every policy mode, custom exact/prefix/substring/args rules, wrappers, redirects, heredocs, YOLO behavior, evaluator denial and failure, and concurrent confirmations. |
| Images | **Partial** | Image file detection, normalization to JPEG, delivery to a vision model, omission for a non-vision model, and base64 exclusion from transcripts. | Add every supported input format, configured dimensions and byte limits, invalid and pathological images, multiple images, and provider-specific serialization for Responses, Anthropic, and Google. |
| Filesystem and secret boundaries | **Partial** | Workspace path escape rejection, hidden and ignored filtering, full-output recovery under an allowed temporary root, and environment-secret redaction from requests and transcripts. | Add process tests for symlink leaves and cycles, absolute read roots, write races, oversized walks, credential-bearing error URLs, auth-header suppression, and redaction across chunk boundaries. |
| HTTP and SSE transport | **Partial** | Real local HTTP requests, provider headers and paths, SSE event names, normal terminal events, non-2xx responses, and one truncated Responses stream. | Add fragmented network reads, multiline and malformed SSE frames, invalid UTF-8 and JSON, premature EOF for every API, oversized error and discovery bodies, connection failure and timeout, URL redaction, and backpressure. |
| Diagnostics and resource lifecycle | **Partial** | Debug and verbose toggles, transcript result release, and large tree-picker memory release are exercised. | Add debug counters, long-stream memory bounds, repeated session and model switches, dropped consumers, temporary log cleanup, graceful live-run shutdown, and leak checks outside Linux/glibc. |
| Build and packaging | **Missing** | None. The E2E suite runs Cargo-built test binaries. | Add release-binary smoke tests, install-layout checks, Nix build checks, and checks on each supported Unix target. |
| QuickJS sandbox limits and unusual value conversion | **Missing** | None. E2E tests use ordinary object results but do not force a sandbox limit or unusual JavaScript value. | Heap, stack, CPU, log, conversion, opaque-value, `lofi_strings`, and guest-cancellation limits remain in `lofi-code` integration tests only. |
| State directories and temporary leases | **Missing** | None. Fixtures redirect state into a temporary root but do not assert its lifecycle. | Add process tests for directory and file permissions, workspace-key collisions, lease cleanup on normal and abnormal exit, stale temporary directory collection, and `LOFI_STATE_HOME` and XDG precedence. |
| Transcript compatibility and corruption | **Missing** | None. Lifecycle tests create well-formed transcripts in the current format. | Add process fixtures for files without cursor records, truncated final lines, malformed events, unsupported versions, invalid parent links, unreadable files, and concurrent writers. |
| Usage, pricing, and cost display | **Partial** | Provider usage fields are asserted for Responses, Anthropic, and Google. | Add process tests for accumulated cost, cache-read and cache-write prices, per-request prices, fallback rates, footer totals, and restore after resume or compaction. |
| Terminal protocol and platform matrix | **Partial** | Linux PTY startup, input, paste, OSC 52 copy, cancellation, and shutdown are exercised; one memory-release test is Linux/glibc-only. | Add macOS CI, other Unix targets, resize and focus events, OSC 11 color reports, split CSI sequences, terminal restoration after signals and startup failures, and non-UTF-8 input. Windows is not supported. |

Run the E2E suite with:

```sh
just e2e
```

See [Development](docs/development.md#end-to-end-tests) for the full check command and test design.

## Usage

### CLI flags

| Flag | Description |
| --- | --- |
| `-p`, `--print <PROMPT>` | Run one non-interactive turn and stream assistant text to stdout. |
| `--list-models` | Print `provider/id — name` lines and exit. |
| `--list-sessions` | Print saved sessions for this workspace and exit. |
| `--model <SPEC>` | Select the model as `provider/model[:level]`. |
| `-c`, `--continue` | Resume the most recent session for this workspace. |
| `--resume <ID>` | Resume a specific session by id prefix. |
| `--no-session` | Do not persist a transcript. |
| `--docs` | Print the embedded API reference index. |
| `--docs-search` | Search the embedded API reference. |
| `--policy-explain` | Evaluate a command against the shell policy (dry-run). |

With no flags, `lofi` launches the interactive TUI against the current working directory as the workspace root.

Model-generated shell commands run on the host through `sh -c`; QuickJS isolation does not sandbox native commands. The default policy asks for confirmation before every such command. See [Shell policy](docs/configuration.md#shell-policy) before selecting a more permissive approval mode.

### TUI keybindings

| Key | Action |
| --- | --- |
| `Enter` | Submit the input box as a prompt. |
| `Ctrl+C` | Cancel the in-flight run. |
| `Ctrl+D` | Quit. |
| `Esc` | Clear the input box. |
| `Up` / `Down` | Scroll the message log. |

### Shell commands

Prefix input with `!` to run it directly through `sh -c` in the workspace. The captured output is included in subsequent model context; use `!!` instead to run it without adding it to context. Direct shell commands have no wall-clock timeout, are persisted in session transcripts, and can be cancelled with `Ctrl+C`.

### Slash commands

| Command | Action |
| --- | --- |
| `/help` | Show the keybinding and command reference. |
| `/clear` | Drop all turns from the log (retain the transcript). |
| `/compact` | Fold the older history into a structured summary. |
| `/model` | Open a model picker. |
| `/resume` | Open a sessions picker. |
| `/tree` | Open a rollback picker. |
| `/session` | Print the session path, message count, and model. |
| `/quit` | Exit. |

## Configuration

See **[Configuration](docs/configuration.md)**.

## Paths

Lofi makes a distinction between user configuration and harness-managed state:

- `${XDG_CONFIG_DIR}/lofi` (usually `~/.config/lofi`) for user-edited configuration (`config.toml`, `AGENTS.md`, `skills/`).
- `${XDG_STATE_HOME}/lofi` (usually `~/.local/state/lofi`) for other agent state.

## Acknowledgements

Lofi builds on ideas from several earlier projects:

- **[pi](https://github.com/earendil-works/pi)**
- **[pi-fabric](https://github.com/monotykamary/pi-fabric)**
- **[pi-vcc](https://github.com/monotykamary/pi-vcc)**
