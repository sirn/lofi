# End-to-end testing

Lofi's process-level test target is defined in `lofi/Cargo.toml` and implemented in `lofi/e2e/`. It runs the Cargo-built `lofi` binary in print mode or through a pseudo-terminal. Tests use temporary configuration, policy, state, and workspace directories plus a local mock model server. They do not need API keys or external network access.

The current target contains 171 tests.

## Run the suite

Run all E2E tests:

```sh
just e2e
```

Run one module or one test with Cargo's name filter:

```sh
cargo test -p lofi --test e2e --offline providers::
cargo test -p lofi --test e2e --offline responses_idle_body_retries_with_the_configured_timeout
```

List the tests without running them:

```sh
cargo test -p lofi --test e2e --offline -- --list
```

Run the complete format, lint, and workspace test set with `just check`.

## Test design

The shared support code in `lofi/e2e/support.rs` provides:

- `Fixture` for isolated config, policy, state, and workspace paths.
- `MockServer` and `MockResponse` for ordered HTTP and SSE responses.
- Print-mode helpers for stdout, stderr, exit status, requests, and transcripts.
- `Tui` for pseudo-terminal input, screen assertions, resize, signals, and OSC output.
- Bounded polling for asynchronous process, transcript, and job state.

The default fixture exposes OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Google Generative AI models through the same local server. It removes common provider API keys from child environments and sets `LOFI_CONFIG`, `LOFI_POLICY`, and `LOFI_STATE_HOME` to temporary paths.

Tests assert public behavior at process boundaries. This includes request payloads, streamed terminal output, exit status, persisted transcript events, process-group cleanup, and behavior after restart. Protocol parser details and hard resource limits stay in crate-level tests when a full process adds no useful coverage.

## Coverage map

The counts below come from the current Cargo test target.

| Module | Tests | Main coverage |
| --- | ---: | --- |
| `agent` | 24 | System prompts and skills, thinking controls, loop recovery, retries, permissions, tool failure, cancellation, and durable partial rounds. |
| `cli` | 13 | Help and version, print mode, informational commands, environment injection, startup failures, session listing, model errors, signals, and reasoning replay. |
| `configuration` | 7 | Automatic approval, resolved and redacted environment values, model discovery and cache fallback, recovery policy, shell-policy modes, and filesystem secret boundaries. |
| `native_tools` | 16 | File, search, docs, skill, recall, result, image, and background-job APIs, including interactive terminal jobs, typed input, key sequences, signals, EOF, dimensions, and idle reporting. |
| `providers` | 11 | All four provider transports, fragmented SSE, custom headers, no-auth requests, premature EOF, response-body idle timeout and retry, lifecycle keepalive chunks, stream errors, tiers, images, and signed reasoning. |
| `sessions` | 51 | Creation, resume, branches, job ownership, failed and cancelled turns, compaction, truncation recovery, transcript repair, state permissions, temporary leases, large transcripts, and restored navigation. |
| `tools` | 10 | Native tool cycles for all providers, parallel and interleaved calls, malformed arguments, thinking signatures, cache breakpoints, compaction, and resume. |
| `tui` | 39 | Transcript details, slash commands, pickers, policy dialogs, queued prompts, user-shell streaming and cancellation, selection and copy, resize, shutdown, and terminal restoration. |
| **Total** | **171** | Real-binary behavior in print mode and pseudo-terminals. |

### Provider and stream coverage

The suite sends real local HTTP requests for all supported APIs. It checks text, reasoning, usage, tools, images, errors, terminal events, and replay data. Transport tests cover fragmented reads, protocol-specific premature ends, retries, and OpenAI Responses lifecycle events that do not map to visible model events. The configured SSE idle timeout resets on each response-body chunk and retries a body that becomes truly idle.

### Session and process coverage

Session tests restart the binary against durable JSON Lines transcripts. They cover active branches, rollback, compaction checkpoints, malformed final lines, missing cursor records, unsupported transcripts, permissions, stale process state, and model restoration. Process tests also check foreground and background process-group cleanup, signal exit status, terminal restoration, and Linux/glibc memory release for large replay and tree-picker fixtures.

### Terminal and job coverage

Pseudo-terminal tests exercise input, paste, resize, OSC 11 and OSC 52 handling, modal priority, transcript navigation, expanded details, and shell output while it is still running. Job tests cover both plain pipes and controlling terminals, including input writes, key encoding, `Ctrl+C`, `Ctrl+D`, terminal dimensions, idle detection, notices, timeout, kill, and shutdown cleanup.

## Current boundaries

The following areas intentionally remain in lower-level tests or need broader process coverage:

- QuickJS heap, stack, CPU, log, conversion, and cancellation limits.
- Malformed UTF-8 SSE, oversized event buffers, and detailed parser permutations.
- Exhaustive configuration precedence, cache corruption, image limits, and pricing totals.
- Exhaustive modal navigation, Unicode width, focus events, and terminal input permutations.
- Concurrent transcript writers and additional transcript migration fixtures.
- Release installation, Nix packaging, and the supported Unix platform matrix.

Two memory-release tests require Linux with glibc. Other E2E tests target Unix process and pseudo-terminal semantics. Windows is not supported.

## Add a test

1. Put the test in the module that owns the public workflow.
2. Reuse `Fixture`, `MockServer`, `MockResponse`, and `Tui`.
3. Keep all network traffic local and all state under the fixture directories.
4. Use bounded wait helpers instead of unbounded sleeps or loops.
5. Assert process-visible behavior and durable state, not private implementation details.
6. Run the focused test, then `just e2e`.
7. Update this page when a module, major coverage area, or documented boundary changes.
