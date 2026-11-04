# Lofi

Lofi is a minimal coding-agent harness written in Rust.

## Platform support

The initial release targets Unix-like systems with POSIX shell and process semantics. Linux is the primary tested platform; macOS and other Unix targets are best effort. Windows is not currently supported.

## Building

```sh
cargo build --release
target/release/lofi
```

## Testing

Run the full format, lint, and test suite:

```sh
just check
```

The deterministic process-level suite runs the real binary against a local mock model server in print mode and a pseudo-terminal. Run it with `just e2e`. It does not need API keys or external network access.

See [End-to-end testing](docs/e2e.md) for the current coverage map, architecture, focused commands, and contribution guidance.

## Usage

### CLI flags

| Flag | Description |
| --- | --- |
| `-p`, `--print <PROMPT>` | Run one non-interactive turn and stream assistant text to stdout. |
| `--model <SPEC>` | Select the model as `provider/model[:level][@tier]`. |
| `-e`, `--env <NAME[=VALUE]>` | Set an environment variable for this instance (repeatable). A bare `NAME` forwards the parent shell's value. |
| `-c`, `--continue` | Resume the most recent session for this workspace. |
| `--resume <ID>` | Resume a specific session by id prefix. |
| `--no-session` | Do not persist a transcript. |
| `--summary <FILE>` | With `--print`, write one JSON record of the run to FILE. See [Run summary](#run-summary). |

### Run summary

`--print --summary <FILE>` writes one JSON object when the run ends.
The text stream on stdout is unchanged.

```sh
lofi --print "Fix the failing test" --summary /tmp/run.json
```

The record holds `ok`, `status` (`completed`, `failed`, or
`incomplete`), `model` (`provider`, `id`, `selected`, `thinking`,
`service_tier`), `rounds`, `tool_calls`, `retries`, `usage` (`input_tokens`,
`output_tokens`, `cache_read_tokens`, `cache_write_tokens`), `cost`,
`subagents` (the same token fields plus `cost`), `stop_reason`,
`duration_ms`, and `error`.
`usage` adds up every round of the turn.
`cost` is the turn's accumulated USD cost. It leaves out `subagents.cost`;
add the two for the run's total spend.
A run that fails writes its record too, even when setup fails before the
first round, so a driver can budget a fleet of runs.
`duration_ms` is the whole run, from process start to exit.
A run that SIGINT or SIGTERM kills dies by that signal and writes no
record. Do not read FILE after a signal kills the run: it can be empty,
hold part of a record, or still hold what it held before the run.
`--summary` without `--print` is rejected before the run starts.

### Subcommands

| Command | Description |
| --- | --- |
| `models` | Print `provider/id — name` lines and exit. |
| `sessions` | Print the saved sessions of this workspace and exit. `--all` covers every workspace and appends each session's directory. |
| `sessions prune` | Delete old session transcripts. `--delete-older-than <AGE>` defaults to `7d`, `--dry-run` lists without deleting, `--all` covers every workspace instead of this one. |
| `docs` | Print the embedded API reference index. |
| `docs search <QUERY>` | Search the embedded API reference. |
| `policy explain <COMMAND>` | Evaluate a command against the shell policy without running it. |
| `policy calibrate` | Replay, probe, or sweep the auto-mode decision gate. See [Automatic approval](docs/configuration.md#automatic-approval). |
| `mcp serve` | Speak the exec bridge protocol on stdio; the harness spawns it for an agent backend. |

With no flags or subcommand, `lofi` launches the interactive TUI against the current working directory as the workspace root.

Model-generated shell commands run on the host through `sh -c`; QuickJS isolation does not sandbox native commands. The default policy asks for confirmation before every such command. See [Shell policy](docs/configuration.md#shell-policy) before selecting a more permissive approval mode.

### TUI keybindings

| Key | Action |
| --- | --- |
| `Enter` | Send a prompt, or queue it while a run is active. |
| `Alt+Enter` / `Ctrl+J` | Insert a newline. |
| `Alt+Up` | Restore the latest queued user prompt for editing. |
| `Up` / `Down` | Move between input lines; recall history at an edge. |
| `PageUp` / `PageDown` | Scroll in Input mode; move by a page in Navigate mode. |
| `Tab` | Switch between Input and Navigate modes. |
| `Esc` | Cancel the active run; otherwise clear input. |
| `Ctrl+C` | Clear input, cancel the active run, or quit after a second press when idle and empty. |
| `Ctrl+D` | Delete the next character; cancel an active run or quit when input is empty. |
| `Ctrl+B` | Open the background dialog on the most recently started work (empty prompt only). |

### Shell commands

Prefix input with `!` to run it directly through `sh -c` in the workspace. The captured output is included in subsequent model context; use `!!` instead to run it without adding it to context. User shell commands have no wall-clock timeout, are persisted in session transcripts, and can be cancelled with `Ctrl+C`.

### Slash commands

| Command | Action |
| --- | --- |
| `/help` | Show keybindings and commands. |
| `/clear` | Clear the transcript log. |
| `/compact` | Fold older history into a summary. |
| `/debug` | Toggle resource diagnostics. |
| `/recall [query]` | Search session history, including compacted content. |
| `/new` | Start a fresh session. |
| `/resume` | Pick a past session. |
| `/tree` | Roll back to a past turn. |
| `/session` | Show session information. |
| `/model` | Switch the active model. |
| `/policy` | Change the shell approval mode for this session. |
| `/thinking` | Switch the thinking level. |
| `/service` | Switch the service tier. |
| `/theme` | Switch the color scheme for this session. |
| `/job` | Open the background dialog on jobs. |
| `/subagent` | Open the background dialog on subagents. |
| `/timer` | Open the background dialog on pending timers. |
| `/quit` / `/exit` | Exit Lofi. |

## Configuration

See **[Configuration](docs/configuration.md)**.

## Paths

Lofi makes a distinction between user configuration and harness-managed state:

- `${XDG_CONFIG_HOME}/lofi` (usually `~/.config/lofi`) for user-edited configuration (`config.toml`, `policy.toml`, `AGENTS.md`, and `skills/`).
- `${XDG_STATE_HOME}/lofi` (usually `~/.local/state/lofi`) for sessions, discovery cache files, and temporary state.

## Acknowledgements

Lofi builds on ideas from several earlier projects:

- **[pi](https://github.com/earendil-works/pi)**
- **[pi-fabric](https://github.com/monotykamary/pi-fabric)**
- **[pi-vcc](https://github.com/monotykamary/pi-vcc)**
