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

### Coverage summary by component

Some workflows cross component boundaries. The detailed matrix puts each workflow under its primary implementation owner.

| Component | Full | Partial | Missing |
| --- | ---: | ---: | ---: |
| [`lofi`](lofi/) | 1 | 0 | 0 |
| [`lofi-core`](lofi-core/) | 5 | 6 | 2 |
| [`lofi-code`](lofi-code/) | 2 | 2 | 1 |
| [`lofi-providers`](lofi-providers/) | 5 | 1 | 0 |
| [`lofi-types`](lofi-types/) and [`lofi-error`](lofi-error/) | 1 | 1 | 0 |
| [`lofi-ui`](lofi-ui/) | 1 | 4 | 0 |
| Workspace | 0 | 0 | 1 |
| **Total** | **15** | **14** | **4** |

#### [`lofi`](lofi/) — executable entry points

This component owns process bootstrap and runtime initialization.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| Process bootstrap | **Full** | Real-binary startup in print and PTY modes, normal exit, malformed-config failure before network or TUI startup, `RUST_LOG`, preservation of a caller-set `MALLOC_ARENA_MAX`, CLI dispatch precedence, and signal exit status. | No principal workflow gaps. Terminal restoration after a forced startup failure remains part of the platform matrix. |

#### [`lofi-core`](lofi-core/) — orchestration, state, and persistence

This component owns agent execution, sessions, compaction, configuration, model selection, images, recall, and direct shell input.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| Session lifecycle | **Full** | Lazy creation, `/new`, `--continue`, explicit and picker resume, latest-session selection, workspace isolation, model restore and override, no-model transcript access, and restart recovery after failed, cancelled, or interrupted turns. | None known for the supported public lifecycle. |
| Session branches and job ownership | **Full** | Rollback, active-branch persistence, sibling exclusion, old-branch retention, job ownership across later calls, off-lineage job cleanup, and stale jobs after process restart. | None known for the supported public lifecycle. |
| System prompt assembly | **Full** | Global, project, and nested `AGENTS.md`; outermost-first order; workspace skill override; namespaced skills; lazy metadata-only skill indexing; de-duplication; project-root detection; and stable replay after resume. | No principal workflow gaps. Invalid or oversized skill files, scan limits, empty files, and uncommon project-root markers remain lower-level cases. |
| Agent failure and recovery | **Full** | Transient retry, authentication failure without retry, configured retry exhaustion, premature-stream retry, cancellation during a retry delay, tool failure recovery, failure after a completed tool round, running-tool cancellation, and durable prompts before provider response. | No principal workflow gaps. Idle timeout and dropped-consumer cases remain lower-level tests. |
| Direct shell input | **Full** | `!` and `!!` context control, transcript persistence and restart replay, non-zero exit, signal death, bounded large output, cancellation, process-group cleanup, and TUI shutdown while a command is running. | No principal workflow gaps. Direct shell commands intentionally use the caller environment; configured secret stripping and redaction apply to model-run shell tools. |
| Compaction | **Partial** | Manual, automatic soft-threshold, hard-pressure continuation, repeated-pressure cooldown, failed compaction recovery, resume, branch selection, and preservation of the latest tool cycle. | Add tiered retention settings, compact-all, image removal, and summary budget limits. |
| Recall and result recovery | **Partial** | TUI query recall, native recall by query, exact tool-result recovery by event id, and explicit unavailability with `--no-session`. | Add all scope and pagination modes, compacted and off-branch content, missing ids, and truncated values. |
| Model discovery and selection | **Partial** | Static models, remote field and API mapping, online discovery, offline cache fallback, explicit models, picker changes, missing models, and resume overrides. | Add authenticated discovery, cache expiry and corruption, static/discovered merge precedence, default provider and model selection, custom endpoint paths, and ambiguous model queries. |
| Configuration loading | **Partial** | Bash environment files, secret redaction, automatic policy approval, model discovery, custom provider headers, `no_auth`, truncation limits, no-model mode, and selected compaction settings. | Add environment and shell value resolution, `LOFI_CONFIG` and XDG path precedence, invalid TOML, image limits, retry settings, UI theme config, and all default-precedence rules. |
| Images | **Partial** | Image file detection, normalization to JPEG, delivery to vision models in all four provider formats, omission for a non-vision model, and base64 exclusion from transcripts. | Add every supported input format, configured dimensions and byte limits, invalid and pathological images, and multiple images. |
| Diagnostics and resource lifecycle | **Partial** | Debug and verbose toggles, transcript result release, and large tree-picker memory release are exercised. | Add debug counters, long-stream memory bounds, repeated session and model switches, dropped consumers, temporary log cleanup, graceful live-run shutdown, and leak checks outside Linux/glibc. |
| State directories and temporary leases | **Partial** | Process tests cover private permissions, startup repair, and stale versus locked temporary leases. | Add workspace-key collision assertions, abnormal exit variants, and `LOFI_STATE_HOME` and XDG precedence. |
| Transcript compatibility and corruption | **Partial** | Resume after a partial final line, fallback without cursor records, malformed and unsupported listing exclusion, private state permissions, and stale temporary-dir collection. | Add process fixtures for malformed middle events, invalid parent links, unreadable files, concurrent writers, and version migrations. |

#### [`lofi-code`](lofi-code/) — sandbox, tools, jobs, and policy

This component owns the QuickJS host, native tools, skills, background jobs, and shell policy.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| Background job API and UI | **Full** | Spawn, status, paged read, wait, notification settings, timeout, kill, missing ids, completion notices, modal list and logs, explicit cleanup, and graceful TUI shutdown with a live process group. | No principal workflow gaps. Exact periodic notice timing, simultaneous-modal layout, and log-window truncation remain lower-level cases. |
| Native file, search, docs, and skill APIs | **Full** | Every public native API: `read`, `ls`, `find`, `grep`, `write`, `edit`, `patch`, `bash`, job APIs, `recall`, `result`, `skills`, `skill`, `docs`, `docsSearch`, and `tmp_dir`; plus success, contained errors, filtering, paging, and truncation recovery. | No principal API gaps. Binary text errors, write races, symlink permutations, and exact walk and result limits remain lower-level cases. |
| Shell policy | **Partial** | Interactive allow and deny, unrestricted fixture execution, automatic model approval, read-only denial, YOLO approval, custom match rules, wrappers, redirects, heredocs, backgrounding, and CLI policy explanation. | Add evaluator denial and failure, concurrent confirmations, and remaining wrapper kinds. |
| Filesystem and secret boundaries | **Partial** | Workspace path escape rejection, hidden and ignored filtering, full-output recovery under an allowed temporary root, symlinked secret rejection, environment-secret redaction from requests and transcripts, and provider auth-header suppression. | Add process tests for symlink cycles, absolute read roots, write races, oversized walks, credential-bearing error URLs, and temporary-log access policy. |
| QuickJS sandbox limits and unusual value conversion | **Missing** | None. E2E tests use ordinary object results but do not force a sandbox limit or unusual JavaScript value. | Heap, stack, CPU, log, conversion, opaque-value, `lofi_strings`, and guest-cancellation limits remain in `lofi-code` integration tests only. |

#### [`lofi-providers`](lofi-providers/) — model APIs and streaming transport

This component owns provider request mapping, stream decoding, tool protocol mapping, and SSE transport.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| OpenAI Chat Completions | **Full** | Text, thinking and usage streaming; all thinking levels; fragmented and parallel tools; text and image tool results; user images; provider errors; retry after premature EOF; fragmented network reads; custom headers; and no-auth requests. | No principal workflow gaps. Malformed frames and provider-specific reasoning-detail signature variants remain lower-level cases. |
| OpenAI Responses | **Full** | Text, reasoning summaries, default summary requests, thinking off, service-tier hints, usage, encrypted reasoning replay, tools, image tool results, failed events, premature-EOF retry, fragmented reads, resume replay, custom headers, and no-auth requests. | No principal workflow gaps. Multiple summary parts and malformed or duplicate output items remain lower-level cases. |
| Anthropic Messages | **Full** | Text, tools, text and image tool results, adaptive thinking, signed thinking replay, usage, cache breakpoints, stream error events, premature-EOF retry, fragmented reads, custom headers, and no-auth requests. | No principal workflow gaps. Parallel and malformed tool calls remain in the provider tool-protocol gap. |
| Google Generative AI | **Full** | Text, function calls and text or image responses, thinking budgets, thought streaming, signatures and replay, usage, native routing, error responses, premature-EOF retry, fragmented reads, custom headers, and no-auth requests. | No principal workflow gaps. Gemini 3 mapping variants, parallel malformed calls, and detailed safety responses remain lower-level cases. |
| Provider tool protocol | **Partial** | Tool execution, text and image result replay for all four APIs, fragmented and parallel OpenAI Chat calls, interleaved Responses calls, malformed OpenAI arguments, cancellation closure after restart, and a valid tool cycle after compaction. | Add parallel and malformed calls for Anthropic and Google, duplicate ids, and orphan results. |
| HTTP and SSE transport | **Full** | Real local HTTP, provider paths and headers, custom headers, auth suppression, SSE event names, fragmented network reads, normal terminal events, non-2xx and in-stream errors, and premature EOF with retry for every provider API. | No principal transport gaps. Malformed or invalid-UTF-8 frames, oversized bodies, idle timeout, URL redaction, and backpressure remain lower-level cases. |

#### [`lofi-types`](lofi-types/) and [`lofi-error`](lofi-error/) — shared contracts

These components own shared request, event, usage, text, recall, and error contracts. `lofi-error` has no standalone public workflow. Its behavior is exercised through the core and provider workflows.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| Thinking-level controls | **Full** | `off`, `low`, `medium`, `high`, and `xhigh`; CLI selection; TUI picker selection; request propagation; resume restore; and explicit override. | None known for the supported levels and controls. |
| Usage, pricing, and cost display | **Partial** | Provider usage fields are asserted for Responses, Anthropic, and Google. | Add process tests for accumulated cost, cache-read and cache-write prices, per-request prices, fallback rates, footer totals, and restore after resume or compaction. |

#### [`lofi-ui`](lofi-ui/) — CLI presentation and terminal UI

This component owns CLI parsing and output, interactive input, commands, modals, transcript rendering, and terminal protocol handling.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| CLI entry points | **Full** | Help, version, print mode, model selection and listing, session listing, invalid resume ids, docs and search, policy explanation, no-TTY operation, dispatch precedence, malformed configuration, provider failures, and signal exit status. | None known for the supported CLI entry points. |
| TUI slash commands and modals | **Partial** | Autocomplete, help, session, theme, model, thinking, service, jobs, debug, verbose, recall, clear, compact, new, resume, tree, quit, exit, unknown commands, and topmost input priority when a policy confirmation overlaps the tree picker. | Add full keyboard navigation, scrolling, copy, cancellation, and narrow-terminal tests for each modal. |
| TUI input and navigation | **Partial** | Prompt submission, queued prompts, bracketed multiline paste, input clearing, quit, cancellation, and raw-Markdown copy through OSC 52. | Add process tests for multiline editing, history recall keys, all Navigate/Select motions, resize reflow, Unicode width, focus events, and malformed or split terminal input sequences. |
| Transcript rendering | **Partial** | Streamed text and thinking, tool blocks, status transitions, selected raw Markdown, and resumed transcript visibility are exercised through the TUI. | Markdown structures, links, tables, code blocks, wrapping, themes, viewport anchoring, and resize behavior remain unit-only. |
| Terminal protocol and platform matrix | **Partial** | Linux PTY startup, OSC 11 answer handling, input, paste, OSC 52 copy, cancellation, shutdown, SIGTERM restoration, and resize reflow are exercised; one memory-release test is Linux/glibc-only. | Add macOS CI, other Unix targets, focus events, split CSI sequences, startup failure restoration, and non-UTF-8 input. Windows is not supported. |

#### Workspace — build and packaging

These checks cover the assembled workspace rather than one Rust crate.

| Area | Status | Covered by E2E | Remaining E2E gaps |
| --- | --- | --- | --- |
| Build and packaging | **Missing** | None. The E2E suite runs Cargo-built test binaries. | Add release-binary smoke tests, install-layout checks, Nix build checks, and checks on each supported Unix target. |

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
