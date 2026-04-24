# Rat

**A terminal session multiplexer built for cloud workflows.** Rat decouples
session state from the host process so your shell sessions survive daemon
restarts, terminal closures, and flaky network connections — and can be
replayed later as an event stream.

```
╭────────────────────────────────────────────────────────────╮
│                                                            │
│   █▀█ ▄▀█ ▀█▀                                              │
│   █▀▄ █▀█ ░█░   rat v0.2.1 · cloud-terminal multiplexer    │
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

### One-liner (prebuilt binaries)

```sh
curl -sSfL https://raw.githubusercontent.com/seamoss/rat/dev/install.sh | sh
```

Detects your OS + arch, pulls the latest release tarball from GitHub,
and drops `rat` + `rat-daemon` into `~/.local/bin`. Supported targets:
`x86_64` / `aarch64` on Linux (glibc) and macOS.

Overrides:

```sh
# Install a specific version
RAT_VERSION=rat-v0.3.0 curl -sSfL https://raw.githubusercontent.com/seamoss/rat/dev/install.sh | sh

# Install somewhere else
RAT_PREFIX_INSTALL=/usr/local/bin curl -sSfL https://raw.githubusercontent.com/seamoss/rat/dev/install.sh | sudo sh
```

### From source

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
# Press Ctrl-A then d to detach. The daemon keeps running.
# Ctrl-A then s hops between sessions; Ctrl-A then ? prints the full cheatsheet.

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

### `rat` (no subcommand)

Spawn a detached session with a fresh UUID, print the UUID to stdout, and
exit without attaching. Useful for scripts and for "give me a session I
can pick up later":

```sh
$ id=$(rat)
$ echo "$id"
f898f408-98a6-4c89-811f-aa3ea6f7eecf
$ rat attach "$id"
```

Respects the nested-session warning. For named or custom-command creates,
use `rat new`.

### `rat new [-n, --name NAME] [-f, --force] [-- CMD [ARGS...]]`

Create a new session and attach to it. The daemon forks itself into a new
session (`setsid`), so closing your terminal won't take it down.

- `--name NAME` (short `-n`): human-friendly label. Names must be unique
  among live sessions. Exported as `$RAT_NAME` to the child.
- `--force` (short `-f`): skip the nested-session warning (see
  [Nested sessions](#nested-sessions)).
- Anything after `--` becomes the command to run. Defaults to `$SHELL`.

### `rat attach ID_OR_NAME [-f, --force]`

Attach to an existing session. `ID_OR_NAME` can be:

- A full UUID
- A unique UUID prefix (e.g., `a4b2`)
- A session name (e.g., `agent`)

Exact name match wins over prefix match if both would apply. `--force`
(short `-f`) skips the nested-session warning.

### `rat list [-f, --force]`

Show running sessions. In a TTY, pops up an arrow-key picker: ↑/↓ (or
`j`/`k`) to navigate, Enter attaches, `Esc`/`q`/`Ctrl-C` cancels. When the
output is piped, prints a plain text table so scripts continue to work.
`--force` applies to the attach that follows a picker selection.

### `rat rename ID_OR_NAME NEW_NAME`

Change a live session's primary name. The new name must be unique among
live sessions (including aliases).

```sh
rat rename agent staging
```

**Caveat:** `$RAT_NAME` inside the already-running shell was exported at
PTY spawn time and cannot be mutated from outside. Prompts decorated via
`rat init` will keep showing the old name until the shell restarts.

### `rat alias ID_OR_NAME ALIAS`

Add a secondary label that also resolves to the session. Useful when you
want a long descriptive name and a short handle:

```sh
rat new -n processing-pipeline
rat alias processing-pipeline pp
rat attach pp      # resolves to processing-pipeline
```

Aliases share the same namespace as names; they must be unique among live
sessions. They disappear when the session ends.

### `rat kill ID_OR_NAME [-y, --yes]`

Terminate a session. Prompts `Are you sure? [y/N]` (default No) unless
`--yes` is passed. Sends SIGTERM to the daemon, waits up to 2s, escalates
to SIGKILL if needed. Stale socket / metadata files get swept on the way
out, so running against a crashed-daemon session cleans up too.

### `rat killall [-y, --yes]`

Nuke every live rat session at once and sweep any stale daemon state.
Prints a table of what's about to die, prompts `Are you sure? [y/N]`
(default No), then SIGTERMs every daemon in parallel with a single 2s
wait before escalating holdouts to SIGKILL. If you're currently inside
a rat session when you run this, the prompt calls that out — you're
about to kill the daemon under your own feet.

### `rat resurrect SOURCE [-n, --name NAME] [-f, --force]`

Spin up a fresh daemon whose event log is pre-seeded with an old
session's `PtyOutput`, then attach. The original PTY is long gone, so
the new shell is fresh — but the scrollback you left behind is
replayed into place, followed by a yellow `-- rat resurrect:
previous session replayed above --` separator and the live prompt.

`SOURCE` is either:
- a `.log` file path (absolute or relative), or
- a session name / alias / UUID / UUID prefix that still has a
  meta file on disk (dead sessions qualify — metas are only removed
  by clean shutdown or `rat kill`).

```sh
# By path — works even if the meta is gone
rat resurrect ~/.local/state/rat/<uuid>.log

# By name — resolves via the still-present meta
rat resurrect agent

# Name the resurrected session differently
rat resurrect agent -n agent-ii
```

What you get back: visual history. What you don't: the old process,
the old environment, the old working directory. Resurrect rebuilds
"what I was looking at," not "what I was running."

### `rat replay LOG_PATH`

Non-interactive playback of a session log file (the
`~/.local/state/rat/<uuid>.log` files). Writes the raw PTY output stream
(including terminal control sequences and colors) to stdout, so piping it
back through a terminal shows the transcript as it happened.

### `rat init SHELL`

Print shell integration code to stdout. `SHELL` is `zsh`, `bash`, or
`fish`. Intended for an `eval` in your rc file.

### `rat completions SHELL`

Print a shell completion script to stdout. Supports `bash`, `zsh`,
`fish`, `elvish`, and `powershell`. Tab-completes subcommands,
option flags, and anywhere a shell value is expected.

One-shot (this shell session only):

```sh
eval "$(rat completions zsh)"
```

Persistent — drop it in your shell's completion directory. Examples:

```sh
# zsh (pick a dir already on $fpath; ~/.zfunc is a common choice)
mkdir -p ~/.zfunc
rat completions zsh > ~/.zfunc/_rat
# ensure fpath + autoload in ~/.zshrc if you haven't already:
#   fpath=(~/.zfunc $fpath); autoload -U compinit && compinit

# bash
rat completions bash > ~/.local/share/bash-completion/completions/rat

# fish
rat completions fish > ~/.config/fish/completions/rat.fish
```

## Keybindings

Rat uses a prefix chord for its own commands, in the tradition of `screen`
and `tmux`. Default prefix is `Ctrl-A`. Inside an attached session:

| Keys                  | Action                                                       |
| --------------------- | ------------------------------------------------------------ |
| `Ctrl-A` then `d`     | Detach from the session (daemon keeps running)               |
| `Ctrl-A` then `c`     | Detach, spawn a fresh session, and attach to it              |
| `Ctrl-A` then `s`     | Detach and open the session switcher (picker)                |
| `Ctrl-A` then `D`     | Detach and kill the session (prompts to confirm)             |
| `Ctrl-A` then `?`     | Print the chord cheatsheet in-band (session keeps running)   |
| `Ctrl-A` `Ctrl-A`     | Send a literal `Ctrl-A` through to the inner program         |
| `Ctrl-D`              | Normal shell EOF — exits the shell and ends the session      |

All other input is forwarded verbatim to the PTY. Any unrecognized chord
command (e.g., `Ctrl-A x`) is silently swallowed.

`c` and `s` chord commands chain across sessions without leaving `rat`:
`<prefix> s` detaches the current session, shows the picker, and attaches
whichever you pick. `<prefix> c` detaches and immediately attaches to a
fresh session.

### Why a chord, not a single key?

A lot of modern TUIs (Claude Code, editors using kitty's CSI-u or xterm's
modifyOtherKeys) enable keyboard-encoding protocols that re-encode every
keypress — including `Ctrl-<anything>` — into multi-byte escape sequences.
A single-byte detach key gets silently swallowed in that regime. A two-key
chord survives because the command key (`d`) is still distinguishable even
if the prefix's on-the-wire encoding shifts.

### Customizing the prefix

Set `RAT_PREFIX` in your environment before `rat new` / `rat attach`. Two
prefix families are supported:

```sh
export RAT_PREFIX=C-b         # Ctrl-b (tmux-style)
export RAT_PREFIX=C-Space     # Ctrl-Space / Ctrl-@ — kinder on the pinky
export RAT_PREFIX=M-a         # Alt-a / Meta-a — no Ctrl stretch at all
rat new -n agent
```

Supported values:

- **Ctrl form:** `C-a` through `C-z`, `C-Space` (alias: `C-@`), plus `C-\`,
  `C-]`, `C-^`, `C-_`.
- **Meta form:** `M-<letter>` or `M-<digit>` (also spelled `Alt-…` or
  `Meta-…`).

An invalid value causes `rat attach` to fail fast before entering raw mode.

**Meta-prefix caveat:** your terminal must send Alt-`a` as `ESC a`. iTerm2
(*Profiles → Keys → General → Left/Right Option key → Esc+*), GNOME
Terminal, Alacritty, and kitty do this by default or via a single setting.
macOS's stock Terminal.app requires *Profile → Keyboard → Use Option as
Meta key*. If Alt presses look like accented characters instead, the
chord won't fire.

A Meta prefix adds a ~50ms flush timeout on bare `ESC` presses so vim and
readline still get `ESC` delivered promptly. Ctrl-prefixes have zero
latency.

## Nested sessions

`rat new` / `rat attach` / `rat list` detect when you're already inside a
rat session (via `$RAT_SESSION`) and show a warning + `[y/N]` prompt before
proceeding. Nesting isn't blocked — there are legitimate reasons to do it
— but it's rarely what you want: detach chords route to the outermost
client, raw-mode clients stack, and input handling gets confusing.

If you know what you're doing, pass `-f` / `--force` to skip the prompt,
or detach from the outer session first.

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

## Environment variables

Exported by the daemon into each session:

- `RAT_SESSION` — the session's UUID. Always set.
- `RAT_NAME` — the session's name, if one was provided.

Shell integrations (`rat init ...`) read these to customize the prompt.
You can read them directly in scripts too — e.g., `if [ -n "$RAT_SESSION" ]`
to know whether you're inside rat.

Read by the `rat` client:

- `RAT_PREFIX` — override the default `Ctrl-A` detach prefix. See
  [Keybindings](#keybindings).
- `RAT_PASSTHROUGH_KBD` — set to `1` to disable the keyboard-protocol
  stripper. By default the client swallows `CSI > … u` / `CSI = … u` /
  `CSI < u` / `CSI ? … u` (kitty keyboard protocol) and `CSI > 4 … m`
  (xterm modifyOtherKeys) from the daemon→client byte stream so that
  inner TUIs can't flip your terminal into a mode that re-encodes the
  detach chord. With passthrough on, you get richer keyboard input
  inside apps like Claude Code, but the detach chord may stop working
  while such an app is in the foreground — you'll have to exit the app
  to detach.

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

**A TUI inside my session has weirder-than-usual keyboard behaviour.**
Rat strips kitty-keyboard-protocol and xterm-modifyOtherKeys enable
sequences from PTY output so the detach chord keeps working. If an inner
app depends on those protocols and misbehaves as a result, set
`RAT_PASSTHROUGH_KBD=1` before attaching. Trade-off: the app gets the
richer input, but detaching may only work once you've exited the app.

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
