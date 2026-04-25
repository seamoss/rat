# Rat

**A terminal session multiplexer that puts session state on disk, not in the daemon.**
Kill the daemon and your session survives. Grep your entire history across every session you've ever run. Resurrect a dead session's scrollback into a fresh shell. Watch a live session without touching it. Then detach, reattach, hand it off.

[![release][release-badge]][releases]
[![license][license-badge]][license]
[![platforms][platforms-badge]][releases]

[release-badge]: https://img.shields.io/github/v/release/seamoss/rat?filter=rat-*&label=release&color=orange
[license-badge]: https://img.shields.io/github/license/seamoss/rat?color=orange
[platforms-badge]: https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-orange
[releases]: https://github.com/seamoss/rat/releases
[license]: LICENSE

<!-- x-release-please-start-version -->
```
╭────────────────────────────────────────────────────────────╮
│                                                            │
│   █▀█ ▄▀█ ▀█▀                                              │
│   █▀▄ █▀█ ░█░   rat v0.4.1 · cloud-terminal multiplexer    │
│                                                            │
╰────────────────────────────────────────────────────────────╯
```
<!-- x-release-please-end -->

## Install

One line, prebuilt binaries, no Rust toolchain required:

```sh
curl -sSfL https://raw.githubusercontent.com/seamoss/rat/dev/install.sh | sh
```

Detects your OS + arch, pulls the latest release tarball, drops `rat` + `rat-daemon` into `~/.local/bin`. Overrides: `RAT_VERSION=rat-v0.4.1`, `RAT_PREFIX_INSTALL=/usr/local/bin`, `RAT_REPO=your/fork`.

From source:

```sh
git clone https://github.com/seamoss/rat.git && cd rat
cargo build --release
ln -sf "$PWD/target/release/rat"        ~/.local/bin/rat
ln -sf "$PWD/target/release/rat-daemon" ~/.local/bin/rat-daemon
```

Shell integration — one `eval` in your rc file, your prompt gains an orange `[rat]` tag whenever you're inside a session:

```sh
eval "$(rat init zsh)"        # ~/.zshrc
eval "$(rat init bash)"       # ~/.bashrc
rat init fish | source        # ~/.config/fish/config.fish
```

Tab-completion — pick your shell, drop the output in the completion dir:

```sh
rat completions zsh > ~/.zfunc/_rat
rat completions bash > ~/.local/share/bash-completion/completions/rat
rat completions fish > ~/.config/fish/completions/rat.fish
```

**Platforms:** native on macOS and Linux, both `x86_64` and `aarch64`. Windows is roadmap — `portable-pty` works there already, we just need named pipes in place of Unix sockets and a different detach strategy. Until then, any modern terminal emulator works: iTerm2, Alacritty, kitty, wezterm, GNOME Terminal, Terminal.app — rat specifically handles kitty's CSI-u keyboard protocol and xterm's `modifyOtherKeys` so chords keep working inside TUIs like Claude Code, helix, and vim.

## Why rat

Tmux and screen are great tools. They solve the problem rat leaves alone (panes, windows, status bars, rich in-session UI) and miss the problem rat solves (your sessions are *valuable* and should behave like data, not like daemon-bound processes).

Rat's wedge:

> **Session state lives in an append-only event log on disk, not in the daemon's memory.**

Every keystroke, every byte of shell output, every resize — all events, all serialized, all replayable. The daemon is a well-behaved consumer of the log, not its owner. Kill the daemon and the session transcript is still there. Start a new daemon and it can pick up where the last one left off.

That single architectural choice cascades into features tmux and screen can't offer without a rewrite:

- **`rat grep`** searches the full transcript of any session — live, detached, or long-dead — the way you'd grep a file. Tmux's scrollback is in RAM; you can't grep last Tuesday's session.
- **`rat resurrect`** replays a dead session's scrollback into a fresh daemon so you can pick up where a crash, reboot, or `kill -9` left you. Tmux has no concept of "previous session."
- **`rat replay`** is bit-exact offline playback of a log file, control sequences and all — pipe it to a terminal and watch the session happen again.
- **`rat watch`** attaches read-only so you can monitor a session without the raw-mode contract or any risk of sending stray input. Pair programming, over-the-shoulder review, CI-style live capture — all cleanly separated from interactive attach.

The event log also sets up everything on the roadmap: durable state across hosts, session migration, compaction/snapshots, log-backed scrollback search. None of this needs a protocol change — it's all just different consumers of the same log format.

### Comparison

| Capability                            |  rat  |  tmux  | screen | zellij |
| ------------------------------------- | :---: | :----: | :----: | :----: |
| Detached sessions                     |   ✅  |   ✅   |   ✅   |   ✅   |
| Multi-client attach                   |   ✅  |   ✅   |   ❌   |   ✅   |
| Session survives daemon crash         |   ✅  |   ❌   |   ❌   |   ❌   |
| Grep historical sessions              |   ✅  |   ❌   |   ❌   |   ❌   |
| Resurrect a dead session's scrollback |   ✅  |   ❌   |   ❌   |   ❌   |
| Offline replay of a session log       |   ✅  |   ❌   |   ❌   |  part  |
| Read-only observer (no keystrokes)    |   ✅  |   ❌   |   ❌   |   ❌   |
| Session naming + aliases              |   ✅  |   ✅   |   ✅   |   ✅   |
| Interactive session picker            |   ✅  |  tab  |   ❌   |   ✅   |
| One-line prebuilt-binary install      |   ✅  | via pkg | via pkg | via pkg |
| Panes / windows / layouts             |   ❌  |   ✅   |   ✅   |   ✅   |
| Copy-mode text selection              |   ❌  |   ✅   |   ✅   |   ✅   |
| Status bar                            |   ❌  |   ✅   |   ✅   |   ✅   |
| Scripting language                    |   ❌  |   ✅   |   ✅   |  part  |
| Windows support                       |  soon  |   ❌   |   ❌   |   ❌   |
| macOS / Linux (x86_64 + aarch64)      |   ✅  |   ✅   |   ✅   |   ✅   |

If you live inside a single terminal window and need tiling, tmux is still the right tool. If your sessions outlive any particular machine or terminal window and you want them to behave like searchable, replayable, resurrectable artifacts — that's rat.

## Quick start

```sh
rat new -n agent                    # new named session, you're attached
echo "$RAT_NAME / $RAT_SESSION"
# press Ctrl-A then d to detach

rat list                            # arrow-key picker in a TTY, plain table when piped
rat attach agent                    # by name …
rat attach a4b2                     # … or UUID prefix

rat watch agent                     # read-only tail from another terminal
rat grep ERROR -s agent             # search one session's full history
rat grep ERROR                      # search every session, ever
rat resurrect agent                 # dead session? replay its scrollback into a fresh shell
rat killall                         # SIGTERM every daemon with confirm + stale sweep

rat replay ~/.local/state/rat/<uuid>.log   # offline playback, control codes intact
```

## Features

### Session management

- **`rat new [-n NAME] [-f] [-- CMD ...]`** — spawn a detached session and attach. Daemon `setsid`s itself so closing the terminal doesn't kill it.
- **`rat attach ID_OR_NAME [-f]`** — by full UUID, unique UUID prefix, name, or alias.
- **`rat list [-f]`** — arrow-key picker in a TTY (`↑↓`/`jk`, `Enter`, `q`/`Esc`/`Ctrl-C`); plain text table when piped for scripting.
- **`rat` (no subcommand)** — spawn a fresh detached session, print the UUID, exit without attaching. Useful in scripts: `id=$(rat)`.
- **`rat rename ID NEW_NAME`** / **`rat alias ID NEW_ALIAS`** — labels; aliases share the namespace and must be unique among live sessions.
- **`rat kill ID [-y]`** / **`rat killall [-y]`** — graceful shutdown (SIGTERM → 2s → SIGKILL) with stale-state cleanup. `killall` flags the ambient session in the prompt so you know you're about to drop yourself.

### Observation & introspection

- **`rat watch ID`** — read-only tail. No input, no resize, no detach sent. Ctrl-C to stop. Pipe-friendly: `rat watch agent | tee agent.log`.
- **`rat grep PATTERN [-s ID] [--log PATH] [--raw]`** — substring search over `PtyOutput` across any log. ANSI-stripped by default; `--raw` matches bytes. Output `{short}:{seq}: {line}` composes with unix pipes.
- **`rat replay LOG_PATH`** — bit-exact playback of a `.log` file to stdout; pipe through a terminal to watch the session as it happened.
- **`rat resurrect SOURCE [-n NAME] [-f]`** — spawn a fresh daemon whose log is pre-seeded with an old session's `PtyOutput`, then attach. `SOURCE` is a `.log` path or a session name/id for dead sessions still having a meta on disk. Visible scrollback is restored; shell, env, and cwd are fresh.

### Keybindings

Chord = press and release the prefix, then press the command key. Default prefix is `Ctrl-A`.

| Keys                  | Action                                                         |
| --------------------- | -------------------------------------------------------------- |
| `Ctrl-A` `d`          | Detach (daemon keeps running)                                  |
| `Ctrl-A` `c`          | Detach, spawn a fresh session, attach it                       |
| `Ctrl-A` `s`          | Detach, open the session switcher, attach the pick             |
| `Ctrl-A` `D`          | Detach and kill (prompts to confirm)                           |
| `Ctrl-A` `[`          | Enter copy mode — scroll the client-side scrollback buffer     |
| `Ctrl-A` `?`          | Print the chord cheatsheet in-band                             |
| `Ctrl-A` `Ctrl-A`     | Send a literal `Ctrl-A` to the inner program                   |
| `Ctrl-D`              | Normal shell EOF — exits the shell and ends the session        |

Inside copy mode:

| Key                     | Action                           |
| ----------------------- | -------------------------------- |
| `j` / `↓`               | Line down                        |
| `k` / `↑`               | Line up                          |
| `Space` / `PgDn` / `^F` | Page down                        |
| `b`    / `PgUp` / `^B`  | Page up                          |
| `g` / `G`               | Jump to oldest / newest          |
| `q` / `Esc`             | Exit; main screen catches up     |

The scrollback buffer is client-side and capped at 1 MiB (`RAT_SCROLLBACK_BYTES` to override). ANSI is stripped for rendering stability — use `rat grep` if you want coloured-transcript search.

#### Customizing the prefix

Set `RAT_PREFIX` before `rat new` / `rat attach`. Two families:

```sh
export RAT_PREFIX=C-b         # Ctrl-b (tmux-style)
export RAT_PREFIX=C-Space     # Ctrl-Space / Ctrl-@ — no pinky stretch
export RAT_PREFIX=M-a         # Alt-a / Meta-a — no Ctrl at all
```

Supported: `C-a..C-z`, `C-Space`, `C-@`, `C-\`, `C-]`, `C-^`, `C-_`, `M-<alnum>` (also `Alt-…` / `Meta-…`). Invalid values fail before raw mode so your terminal never wedges.

**Meta-prefix caveat:** your terminal must send Alt-`a` as `ESC a`. iTerm2 → *Profile → Keys → General → Left/Right Option Key → Esc+*; Terminal.app → *Profile → Keyboard → Use Option as Meta*; most others do this by default. A 50 ms flush timeout delivers bare `ESC` promptly so vim/readline aren't starved.

#### Why a chord, not a single key?

Modern TUIs (Claude Code, helix, vim via kitty's keyboard protocol, xterm's `modifyOtherKeys`) re-encode every `Ctrl-<x>` into a multi-byte CSI sequence — a single-byte detach key gets silently eaten. A chord degrades gracefully because the command key (`d`, `c`, `s`, `[`, `?`) is still distinguishable even if the prefix's on-the-wire encoding shifts. Rat's output filter also strips the protocol-enable escape sequences from the daemon→client byte stream by default, so most TUIs can't flip the terminal into a mode that would break the chord in the first place (`RAT_PASSTHROUGH_KBD=1` disables this if you need the richer input encoding more than you need reliable detach).

### Shell integration

`rat init <zsh|bash|fish>` prints a snippet for your rc file. Inside a rat session your prompt gains an orange `[rat]` / `[rat:NAME]` prefix; outside rat, your prompt is untouched. Works by checking `$RAT_SESSION` / `$RAT_NAME`, which the daemon exports to the child PTY at spawn.

## Nested sessions

`rat new` / `rat attach` / `rat list` detect when you're already inside rat (via `$RAT_SESSION`) and show a `[y/N]` confirmation before proceeding. Nesting isn't blocked — there are legitimate reasons (running rat-within-rat across an SSH jump) — but detach chords route to the outermost client, raw-mode clients stack, and input handling gets confusing. `-f` / `--force` skips the prompt.

## Environment

Exported by the daemon into each session:

- `RAT_SESSION` — the session's UUID. Always set.
- `RAT_NAME` — the session's name, if one was given.

Read by the `rat` client:

- `RAT_PREFIX` — override the default `Ctrl-A` chord prefix. See [Keybindings](#keybindings).
- `RAT_SCROLLBACK_BYTES` — copy-mode ring-buffer size (default 1 MiB).
- `RAT_PASSTHROUGH_KBD` — set to `1` to disable the keyboard-protocol stripper. Inner TUIs get richer input; chord detach may stop working until you exit the TUI.

## Filesystem layout

```
$XDG_STATE_HOME/rat   (default: ~/.local/state/rat)
├── <uuid>.log            # per-session event log (JSON-lines, append-only)
├── run/
│   ├── <uuid>.sock       # Unix domain socket for client ↔ daemon
│   └── <uuid>.meta.json  # pid · command · start time · paths · name · aliases
├── daemon.stdout
├── daemon.stderr
└── daemon.trace          # fine-grained daemon tracing (always on)
```

Sock + meta are removed on clean shutdown (or by `rat kill` / `rat killall` on stale state). `.log` files accumulate — they're your scrollback history and are safe to `rm` when you don't need them.

## Architecture (tl;dr)

Two binaries and one library:

- **`rat`** — the interactive CLI, client, and launcher.
- **`rat-daemon`** — the headless session host. Holds the PTY, appends events to the log, listens on a Unix socket, broadcasts live events to attached clients.
- **library** — `event.rs`, `log.rs`, `session.rs`, `paths.rs`, `protocol.rs`.

The daemon reads PTY output, appends each chunk to the session's event log, and broadcasts the logged event to attached clients. Client input flows the other way: read stdin, log as `ClientInput`, write to the PTY.

The event log is the source of truth. Attach replays the log up to a snapped sequence number and then subscribes to the live broadcast for events appended afterwards — no duplicates, no gaps.

Full details: [ARCHITECTURE.md](ARCHITECTURE.md).

## Troubleshooting

**"daemon didn't bind socket within 3s"**
`rat-daemon` failed to start or panicked before binding its socket. Check `~/.local/state/rat/daemon.stderr` and `daemon.trace`.

**The shell I spawn exits immediately.**
Your rc file has an `exit` or a hard error in it. Rat propagates your whole environment to the child, so rc failures fail the shell. `RUST_LOG=debug rat new` for more tracing.

**A stale session shows up in `rat list` marked "dead".**
Daemon crashed without cleanup. `rat kill <id>` sweeps dead-session state even without any live process to signal. `rat killall` sweeps every dead one in a single pass.

**Prompt doesn't show `[rat]` inside a session.**
Make sure `eval "$(rat init ...)"` is in the rc file that runs in interactive shells. For zsh that's `~/.zshrc`; for bash on macOS it's often `~/.bash_profile`. Verify with `echo $RAT_SESSION` — if it's set but the prompt isn't decorated, the integration isn't sourced.

**TUI inside my session has weird keyboard behaviour.**
Rat strips kitty-keyboard-protocol and xterm-`modifyOtherKeys` enable sequences so the detach chord keeps working. If an inner app depends on those protocols and misbehaves, `RAT_PASSTHROUGH_KBD=1 rat attach …` disables the filter.

**Terminal looks wedged after a crash.**
`reset` or `stty sane`. Rat restores cooked mode on Drop, but a hard kill (`kill -9 rat`) bypasses that.

**`rat watch` output doesn't redraw cleanly.**
Watch doesn't enter raw mode or send resizes — if your watching terminal is a different size from the attached one, VT sequences designed for the attacher may render odd. Resize your watching terminal to match.

## Roadmap

In rough priority order:

1. **Windows support.** `portable-pty` works there already; we need named pipes instead of Unix sockets and a non-`setsid` detach strategy. Once shipped, the one-liner installer picks up a Windows target and rat is genuinely cross-platform.
2. **Remote attach** — `rat attach user@host:session`. The socket is host-local today; SSH multiplexing or a small tunnelling protocol unlocks the cloud-terminal pitch.
3. **Binary wire protocol.** JSON-lines is excellent for debugging; postcard/bincode is right for production once the shape stabilizes.
4. **Log compaction + VT-backed snapshots.** Session logs grow unbounded. Periodic snapshots via a VT100/220 emulator (e.g. `vte`) would bound replay cost and unlock a "jump to time" scroll UX.
5. **Panes / windows.** Core multiplexer UX; deliberately deferred while rat focuses on persistence.
6. **Durable state beyond one host.** The log is already the source of truth; putting it on shared filesystem / object store / sqlite replica lets sessions migrate.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for dev setup, code style, PR flow, and the Conventional-Commits grammar the release automation expects. In short: `feat:` bumps minor, `fix:` bumps patch, `feat!:` / `BREAKING CHANGE:` bumps major; don't hand-edit `Cargo.toml`, `Cargo.lock`, or `CHANGELOG.md` — release-please owns all three.

## License

[MIT](LICENSE).
