# Rat

**A terminal session multiplexer built for cloud workflows.** Rat decouples
session state from the host process so your shell sessions survive daemon
restarts, terminal closures, and flaky network connections — and can be
replayed later as an event stream.

```
╭────────────────────────────────────────────────────────────╮
│                                                            │
│   █▀█ ▄▀█ ▀█▀                                              │
│   █▀▄ █▀█ ░█░   rat v0.1.0 · cloud-terminal multiplexer    │
│                                                            │
╰────────────────────────────────────────────────────────────╯
```

## Why another multiplexer?

Tmux's model is brilliant on the machine where you live. It gets rough once
your work leaves that machine:

- **Session state lives in the daemon process.** If the daemon dies, the
  session dies with it — scrollback, environment, everything.
- **Reattaching over flaky networks is fragile.** Mosh fixes the wire, not
  the state model.
- **You can't hand a session off.** No portable artifact to replay, inspect,
  or migrate.

Rat's wedge: **session state lives in an append-only event log on disk, not
in the daemon's memory.** Every keystroke, every byte of shell output, every
resize — all events, all serialized, all replayable. The daemon is a
well-behaved consumer of that log, not its owner. Kill the daemon and the
session transcript is still there; start a new daemon and it can pick up
where the last one left off.

This README covers what's built today. The longer-term vision is in
[ARCHITECTURE.md](ARCHITECTURE.md).

## Status

Rat is at a working MVP. The core thesis is proven end-to-end:

- A detached background daemon holds a PTY, writes an event log, and serves
  clients over a Unix socket.
- Multiple clients can attach to the same session, see the scrollback replay,
  and interact live.
- Sessions survive client disconnect, terminal closure, and `rat kill`.
- Everything that happens is recorded and can be replayed offline from the
  log file.

What it isn't yet: distributed across hosts, remote-attachable over SSH, or
a full TUI with panes. See [Roadmap](#roadmap).

## Features

- **Detached daemon** — sessions outlive the terminal that started them.
- **Reattach from anywhere** — open a new terminal, `rat attach <name>`,
  scrollback replays, you're back.
- **Multi-client attach** — several terminals can observe and drive the same
  session (pair programming, over-the-shoulder review).
- **Event log per session** — full transcript on disk; use `rat replay` to
  play it back offline.
- **Session naming** — `rat new -n agent`, `rat attach agent`. `$RAT_NAME`
  and `$RAT_SESSION` are exported to the child so your shell prompt can
  reflect the session context.
- **Interactive picker** — `rat list` in a TTY shows an arrow-key picker
  into attach; piped output stays plain text for scripts.
- **Graceful teardown** — `rat kill` sends SIGTERM, escalates to SIGKILL on
  timeout, cleans up stale socket/metadata files either way.
- **Shell integration** — `eval "$(rat init zsh|bash|fish)"` decorates your
  prompt inside rat sessions.
- **SIGWINCH propagation** — resize your terminal, the PTY follows.

## Install

Rat is pre-release; build from source.

**Requirements:** Rust 1.80+ (`edition = "2024"` in `Cargo.toml`),
a Unix-like OS (Linux or macOS). Windows is not currently supported.

```sh
git clone https://github.com/seamoss/rat.git
cd rat
cargo build --release
```

This produces two binaries in `target/release/`:

- `rat` — the interactive CLI (client + launcher).
- `rat-daemon` — the background session host, invoked by `rat new`.

Install by copying or symlinking both into a directory on your `PATH` — they
must sit side-by-side (the launcher finds `rat-daemon` next to its own
executable):

```sh
mkdir -p ~/.local/bin
ln -s "$PWD/target/release/rat"        ~/.local/bin/rat
ln -s "$PWD/target/release/rat-daemon" ~/.local/bin/rat-daemon
```

Then either ensure `~/.local/bin` is on your `PATH` or place the symlinks in
a directory that already is (`/usr/local/bin`, etc.).

### Shell integration

Add a prompt decoration so sessions you're attached to are visibly marked:

```sh
# ~/.zshrc
eval "$(rat init zsh)"

# ~/.bashrc
eval "$(rat init bash)"

# ~/.config/fish/config.fish
rat init fish | source
```

Inside a rat session, your prompt gains an orange `[rat]` (or `[rat:NAME]`
if named) prefix. Outside rat, your prompt is untouched.

## Quick start

```sh
# Start a new named session
rat new -n agent

# ... you're in a shell. Inside:
echo "$RAT_NAME / $RAT_SESSION"
ls
# Press Ctrl-\ to detach. The daemon keeps running.

# List live sessions
rat list
# (in a terminal, this pops into an arrow-key picker;
#  Enter on a row attaches to it)

# Reattach by name
rat attach agent

# Reattach by UUID prefix
rat attach a4b2

# Kill a session (with confirmation)
rat kill agent
# Kill this session?
#   id: ...
# Are you sure? [y/N]

# Offline replay of a session log
rat replay ~/.local/state/rat/<uuid>.log
```

## Commands

### `rat new [-n, --name NAME] [-- CMD [ARGS...]]`

Create a new session and attach to it. The daemon forks itself into a new
session (`setsid`), so closing your terminal won't take it down.

- `--name NAME` (short `-n`): human-friendly label. Names must be unique
  among live sessions. Exported as `$RAT_NAME` to the child.
- Anything after `--` becomes the command to run. Defaults to `$SHELL`.

### `rat attach ID_OR_NAME`

Attach to an existing session. `ID_OR_NAME` can be:

- A full UUID
- A unique UUID prefix (e.g., `a4b2`)
- A session name (e.g., `agent`)

Exact name match wins over prefix match if both would apply.

### `rat list`

Show running sessions. In a TTY, pops up an arrow-key picker: ↑/↓ (or
`j`/`k`) to navigate, Enter attaches, `Esc`/`q`/`Ctrl-C` cancels. When the
output is piped, prints a plain text table so scripts continue to work.

### `rat kill ID_OR_NAME [-y, --yes]`

Terminate a session. Prompts `Are you sure? [y/N]` (default No) unless
`--yes` is passed. Sends SIGTERM to the daemon, waits up to 2s, escalates
to SIGKILL if needed. Stale socket / metadata files get swept on the way
out, so running against a crashed-daemon session cleans up too.

### `rat replay LOG_PATH`

Non-interactive playback of a session log file (the
`~/.local/state/rat/<uuid>.log` files). Writes the raw PTY output stream
(including terminal control sequences and colors) to stdout, so piping it
back through a terminal shows the transcript as it happened.

### `rat init SHELL`

Print shell integration code to stdout. `SHELL` is `zsh`, `bash`, or
`fish`. Intended for an `eval` in your rc file.

## Keybindings

Inside an attached session:

| Key      | Action                                                      |
| -------- | ----------------------------------------------------------- |
| `Ctrl-\` | Detach from the session (daemon keeps running)              |
| `Ctrl-D` | Normal shell EOF — exits the shell and ends the session     |

All other input is forwarded verbatim to the PTY.

## Filesystem layout

Rat keeps everything under `$XDG_STATE_HOME/rat` if set, else
`$HOME/.local/state/rat`.

```
~/.local/state/rat/
├── <uuid>.log            # per-session event log (append-only, JSON-lines)
├── run/
│   ├── <uuid>.sock       # Unix domain socket for client ↔ daemon
│   └── <uuid>.meta.json  # pid / command / start time / sock+log paths
├── daemon.stdout         # daemon stdout (usually empty in normal operation)
├── daemon.stderr         # daemon stderr (errors, if any)
└── daemon.trace          # tracing output from the daemon (tokio + rat)
```

Sock and meta files are removed on clean shutdown (or by `rat kill` on
stale ones). Log files accumulate — they're your scrollback history, safe
to clean up with `rm` when you don't need them.

## Environment variables the daemon exports

- `RAT_SESSION` — the session's UUID. Always set.
- `RAT_NAME` — the session's name, if one was provided.

Shell integrations (`rat init ...`) read these to customize the prompt.
You can read them directly in scripts too — e.g., `if [ -n "$RAT_SESSION" ]`
to know whether you're inside rat.

## Architecture (tl;dr)

Rat is split into a small library and two binaries:

- `rat` — the launcher / interactive client
- `rat-daemon` — the headless session host
- shared `rat` library — event types, file-backed event log, wire protocol,
  paths, session metadata

The daemon holds a PTY running your shell. It reads PTY output, appends
each chunk to the session's event log, and broadcasts the logged event to
all attached clients. Client input flows the other way: read from stdin,
log as `ClientInput`, write to the PTY.

The event log is the source of truth. Reattach replays the log (up to a
snapped sequence number) and then subscribes to the live broadcast for
events appended afterward.

Full details: [ARCHITECTURE.md](ARCHITECTURE.md).

## Troubleshooting

**"daemon didn't bind socket within 3s"**
The `rat-daemon` binary failed to start or panicked before binding its
socket. Check `~/.local/state/rat/daemon.stderr` and `daemon.trace` for
the actual error.

**The shell I spawn exits immediately.**
Make sure your `~/.zshrc` / `~/.bashrc` doesn't have an `exit` or a hard
error in it. Rat propagates your whole environment to the child, so if
something in your rc files fails, the shell fails too. Run with
`RUST_LOG=debug rat new` for more tracing.

**I see a stale session in `rat list` marked "dead".**
The daemon crashed or was killed without cleanup. `rat kill <id>` sweeps
dead-session state files even without hitting any live process.

**My prompt doesn't show `[rat]` inside a session.**
Make sure you added the `eval "$(rat init ...)"` line to the rc file that
actually runs in interactive subshells. For zsh that's `~/.zshrc`; for
bash on macOS it's often `~/.bash_profile`. Verify with
`echo $RAT_SESSION` inside the session — if it's set but your prompt is
unchanged, the integration isn't being sourced.

**Terminal looks wedged after a crash.**
Run `reset` or `stty sane`. Rat tries to restore cooked mode on Drop, but
a hard kill (`kill -9` on the client) bypasses that.

## Roadmap

In rough priority order:

1. **Binary wire protocol.** JSON-lines is great for debugging; for
   production the log and wire format should be compact and fast (postcard
   / bincode).
2. **Remote attach.** `rat attach user@host:session` — today the socket is
   host-local. SSH multiplexing or a small TCP-tunneling protocol would
   unlock the cloud-terminal pitch.
3. **Log compaction.** Session logs grow without bound. For long-running
   sessions, snapshot the terminal state periodically so replay doesn't
   scale linearly with session age.
4. **Full terminal emulation for replay.** Today `rat replay` dumps raw
   PTY bytes. A proper VT100/VT220 emulator + "jump to time" would make
   scrollback searchable and give a real session-browser experience.
5. **Multiple panes / windows.** Core terminal-multiplexer UX is still
   missing; rat focuses on persistence first.
6. **Windows support.** `portable-pty` works on Windows but our
   Unix-socket transport and `setsid` detach don't. Named pipes + a
   different detach strategy would bridge this.
7. **Durable state beyond one host.** The log is already the source of
   truth; putting it on a shared filesystem (or object store, or sqlite
   replica) would let sessions migrate between hosts.

## Contributing

Contributions welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for dev
setup, code style, and the PR flow.

## License

[MIT](LICENSE). See LICENSE file for full text.
