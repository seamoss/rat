# Architecture

This document explains how Rat is put together and, more importantly, *why*.
If you're looking for how to use Rat, see the [README](README.md); if you're
looking for how to contribute, see [CONTRIBUTING](CONTRIBUTING.md).

## Table of contents

- [Design thesis](#design-thesis)
- [Components at a glance](#components-at-a-glance)
- [The event log](#the-event-log)
- [Wire protocol](#wire-protocol)
- [Session lifecycle](#session-lifecycle)
- [Concurrency model](#concurrency-model)
- [Filesystem layout and discovery](#filesystem-layout-and-discovery)
- [Security model](#security-model)
- [Design decisions worth calling out](#design-decisions-worth-calling-out)
- [Roadmap and open questions](#roadmap-and-open-questions)

## Design thesis

> **Session state should not be bound to a single process on a single host.**

Tmux's model — one daemon per user on one machine, holding all session state
in memory — is excellent when your work is local. It becomes a problem when:

- The daemon crashes or gets OOM-killed. Sessions vanish.
- You want to hand a session off to another host or another user.
- You want to audit exactly what happened in a session, or replay it, or
  search for something that scrolled off.
- The network between you and the daemon is lossy.

Rat inverts the ownership: the **event log on disk is the source of truth**.
The daemon is a process that *happens* to be running a PTY against that log
right now. Kill the daemon and the log is still there; attach a new client
to a new daemon and replay the log to rebuild the view.

This choice propagates through the whole design:

- Every client action is logged *before* it hits the PTY.
- Every PTY output is logged *before* it's broadcast to clients.
- Reattach is log replay + broadcast subscription. There's no ad-hoc
  "current state" data structure the daemon has to keep synchronized.
- The long-term story (remote attach, host migration, distributed sessions)
  is a *placement* problem for the log, not a rearchitecture of the daemon.

## Components at a glance

```
                    ┌──────────────────────┐
                    │  rat (client CLI)    │
                    │  raw-mode terminal   │
                    └──────────┬───────────┘
                               │
                 framed JSON ↕ │  unix socket
                               │
                    ┌──────────▼───────────┐       ┌──────────────┐
                    │      rat-daemon      │──────▶│ PTY master   │
                    │                      │       └──────┬───────┘
                    │  ┌────────────────┐  │              │
                    │  │ broadcast chan │  │       ┌──────▼───────┐
                    │  │ (LoggedEvent)  │  │       │ PTY slave /  │
                    │  └────────▲───────┘  │       │ child shell  │
                    │           │          │       └──────────────┘
                    └───────────┼──────────┘
                                │ append
                                ▼
                    ┌──────────────────────┐
                    │   <uuid>.log         │
                    │   JSON-lines events  │
                    │   append-only        │
                    └──────────────────────┘
```

Three crates in one Cargo package:

- **`rat`** — the user-facing CLI. Spawns and detaches `rat-daemon`, connects
  to the socket, runs the interactive client loop (raw mode, I/O pumps,
  resize, detach). Also holds the shell-integration emitter (`rat init`).
- **`rat-daemon`** — the headless background process. Owns the PTY, appends
  to the log, accepts client connections on a Unix socket, broadcasts events.
- **`rat` library crate** — shared code: event types, file-backed event log,
  wire protocol + framing, session identity, path layout.

Binary split matters because:

- The daemon has no interactive terminal concerns. It doesn't know about raw
  mode, SIGWINCH from "our" terminal, or key bindings. It just serves.
- The client has no PTY concerns. It doesn't spawn processes or own any
  long-lived state. Kill it, restart it, no harm done.
- This clean seam is what enables remote attach later: the client needs
  only a bidirectional framed transport to the daemon, and today that's a
  Unix socket, tomorrow it could be SSH, a mux over TCP, whatever.

## The event log

The log is the whole bet. It's an append-only file of `LoggedEvent` records,
one JSON object per line (`JSON-lines` format):

```rust
struct LoggedEvent {
    seq: u64,
    timestamp: SystemTime,
    event: Event,
}

enum Event {
    SessionStarted { command, args, env, cwd, cols, rows },
    PtyOutput     { data: Vec<u8> },
    ClientInput   { data: Vec<u8> },
    Resized       { cols, rows },
    SessionEnded  { exit_code: Option<i32> },
}
```

Invariants:

- `seq` is monotonic and gap-free within a session. The next append uses
  `latest_seq()`, which reads the current max and adds 1.
- `timestamp` is captured at append time, not at event-origin time.
- Every state change is an event. Replaying events from seq 0 reconstructs
  the session entirely.
- Writes are flushed after every append. Readers that open the file mid-
  write will see all completed lines; they will not see partial lines
  because we flush line-at-a-time.

`ClientInput` is recorded alongside `PtyOutput` deliberately. It doubles
log volume in the worst case, but gives us:

1. Deterministic replay — the session is the merge of these two streams.
2. A complete audit trail for security review ("what did someone actually
   type?").
3. Primitive pair-programming UX down the road (one client sees what the
   other types).

For the MVP we chose JSON-lines over a binary format because debugging is
paramount at this stage. `cat <uuid>.log | jq` works and has rescued us
multiple times. See the roadmap for the binary migration plan.

### Read-from-log semantics

`FileEventLog::read_from(start_seq)` returns every event with `seq >= start_seq`,
read by streaming the file top-to-bottom. It's O(n) in file size, which is
fine for MVP scrollback replay but won't scale for long-lived sessions.
Log compaction (periodic terminal-state snapshots plus post-snapshot
events) is a roadmap item.

## Wire protocol

Unix-domain stream socket. Every message is length-prefixed JSON:

```
┌────────────┬──────────────────────────┐
│  len (u32) │  JSON-serialized body    │
│  big-endian│  (ClientMsg / DaemonMsg) │
└────────────┴──────────────────────────┘
```

Max frame size: 16 MiB (guards against runaway allocations on a corrupted
wire).

**Client → Daemon (`ClientMsg`):**

| Variant                      | Meaning                                                                             |
| ---------------------------- | ----------------------------------------------------------------------------------- |
| `Attach { from_seq }`        | First message on a connection. Asks for replay from `from_seq` plus live stream.    |
| `Input { data }`             | Keystrokes / bytes to feed to the PTY.                                              |
| `Resize { cols, rows }`      | Window dimensions for the PTY.                                                      |
| `Detach`                     | Clean disconnect (daemon stays running).                                            |

**Daemon → Client (`DaemonMsg`):**

| Variant                              | Meaning                                                                      |
| ------------------------------------ | ---------------------------------------------------------------------------- |
| `Attached { id, name, cols, rows, latest_seq }` | First response. Acknowledges attach; confirms current size and log tip.       |
| `Event { event: LoggedEvent }`       | A replayed or live event.                                                    |
| `SessionEnded { exit_code }`         | Session is over; client should disconnect.                                   |
| `Error { message }`                  | Non-fatal error reported out-of-band.                                        |

### Replay / live boundary

When a client attaches, two things need to line up: the historical events
in the log, and the stream of live events going forward. The race is:

```
t=0   client subscribes to broadcast (bcast_rx is held before handle_client starts)
t=1   client sends Attach{from_seq}
t=2   daemon locks the log
t=3   daemon reads latest_seq()  ← snap
t=4   daemon reads events [from_seq..)
t=5   daemon unlocks the log
```

Between t=3 and t=5 another thread (the PTY reader) could append. That
append is written to the log *and* broadcast. Without care the client
would see it twice.

We handle it with a two-filter rule:

- **Historical stream:** send only events with `seq < snap`.
- **Broadcast stream:** send only events with `seq >= snap`.

Since `snap` was read while the log lock was held, any append that broadcasts
an event with `seq < snap` definitely committed before we read the tip, and
therefore its broadcast was enqueued in the receiver we subscribed to before
we asked for the snapshot. Dropping those from the broadcast stream is safe.

## Session lifecycle

```
   rat new  ─────────┐
                     │ 1. generate session id
                     │ 2. get terminal size
                     │ 3. spawn rat-daemon (detached: setsid,
                     │    stdin=/dev/null, stdout/err → log files)
                     │ 4. poll for socket (up to 3s)
                     │ 5. connect (attach as client)
                     ▼
               rat-daemon
                     │
                     │ PTY spawned, child running,
                     │ event log open
                     │
   rat attach ──────▶│  (additional clients, any time)
                     │
   Ctrl-\ (in any)   │  → client sends ClientMsg::Detach,
                     │    disconnects, daemon keeps running
                     │
   rat kill  ───SIGTERM──▶ daemon → SIGTERM to PTY child
                     │  → child exits
                     │  → child-exit watcher appends SessionEnded
                     │  → daemon removes sock + meta, exits
   shell `exit` ─────▶ PTY child exits (same tail as rat kill)
```

A couple of notes on the less-obvious pieces:

- **Detach from the controlling tty.** `rat new` spawns `rat-daemon` with a
  `pre_exec` closure that calls `setsid(2)`. This is what lets the daemon
  survive closing the original terminal.
- **SessionEnded on graceful shutdown.** The daemon doesn't append
  `SessionEnded` on signal receipt directly. Instead, the signal handler
  forwards SIGTERM to the PTY child; the existing `child.wait()`-watcher
  task sees the exit and runs the normal shutdown path. This keeps a single
  code path responsible for "the session is done."
- **Stale state cleanup.** Daemons that crash don't remove their sock/meta
  files. `rat kill` compensates: if the pid isn't alive, it still sweeps
  the stale files. `rat list` (interactive picker) hides dead sessions.

## Concurrency model

The daemon mixes blocking PTY I/O with async network I/O:

- **Tokio multi-thread runtime** for everything network-facing and the
  socket accept loop.
- **Two dedicated OS threads** for PTY I/O, because `portable-pty` gives
  us blocking `Read` / `Write` handles and mixing those with tokio awaits
  is worse than a thread:
  - `pty-reader` — blocking read on the PTY, append log, broadcast event.
  - `pty-writer` — blocking `mpsc::UnboundedReceiver::blocking_recv`,
    writes bytes to the PTY master.
- **Per-client tokio task** for each connection. Inside it a second
  spawned task handles the broadcast → socket stream so that inbound
  (client → daemon) and outbound (daemon → client) paths don't block each
  other.
- **`Arc<std::sync::Mutex<FileEventLog>>`** gates log append. Critical
  sections are tiny (one file write + flush), so contention is not a
  concern at current scale. We never hold this lock across an `.await`.
- **`Arc<std::sync::Mutex<Box<dyn MasterPty + Send>>>`** for the PTY
  master. Only held during `resize()` calls, which are `&self` on the
  trait and effectively ioctl calls.
- **`tokio::sync::broadcast`** for live event fanout. Buffer size 4096.
  On `Lagged` the forwarder drops the client — MVP simplification; the
  right fix is to re-read from the log and resume broadcast when caught up.

Client side is simpler:

- Tokio runtime.
- One OS thread for reading stdin (blocking `read(2)`) into a channel —
  cleanest way to deal with raw-mode stdin while the rest of the client
  is async.
- Three tokio tasks: output (daemon → stdout), input (stdin channel →
  daemon), resize (SIGWINCH → daemon).
- A `tokio::sync::Notify` signals "we're done" from whichever task decides
  first (session ended, user detached, socket dropped).

## Filesystem layout and discovery

```
$XDG_STATE_HOME/rat                    # or $HOME/.local/state/rat
├── <uuid>.log                         # session event log (append-only)
├── run/
│   ├── <uuid>.sock                    # Unix socket (per session)
│   └── <uuid>.meta.json               # SessionMeta (pid, started, command, sock+log paths, optional name)
├── daemon.stdout                      # daemon's stdout (logs, mostly empty)
├── daemon.stderr                      # daemon's stderr (errors)
└── daemon.trace                       # tokio + rat tracing output
```

Discovery is via scanning `run/*.meta.json` — cheap, no extra index, no
process-listing required. `rat list` loads them, checks liveness via
`kill(pid, 0)`, and sorts by `started`.

Two things are deliberately *not* in the meta file:

- The full command line. Only the executable path. (Reasoning: less
  coupling to user-supplied args in discovery / prompts.)
- Any secrets. The meta file is world-readable within the user's home.
  If something sensitive belongs to the session it belongs in the log,
  not in metadata.

## Security model

Rat today is not a security boundary. Treat it like `script(1)` or
`tmux(1)`:

- Sessions run as the user who invoked `rat new`. The daemon has the same
  privileges as the user.
- Log files include raw PTY output, which means anything typed or echoed
  in the session — including passwords you *thought* would be invisible —
  can end up in plain JSON on disk. Mode is 0644 by default (umask-
  dependent). If this matters to you, `umask 077` before `rat new` or
  tighten the state dir.
- Unix sockets are 0755 by default (umask-dependent), bounded to the
  user's filesystem. There's no remote attach yet, so no network-exposed
  surface.
- `rat kill` uses `kill(pid, 0)` to test liveness, which isn't cross-user
  safe against PID reuse in pathological cases. See the `starttime`-based
  fix in CONTRIBUTING's "where help is wanted."

## Design decisions worth calling out

### Why JSON-lines, not a binary format?

Debuggability during MVP is more valuable than on-disk size or parse
speed. A session's log is human-readable with `jq`. Fixing the format
too early would lock us out of cheap debugging. The roadmap migrates to
`postcard` or `bincode` once the shape of events stabilizes.

### Why two binaries instead of subcommands of one?

`rat new` needs `std::process::Command::spawn` to fork an executable. We
could re-exec ourselves with a `--daemon` flag, but splitting binaries
makes the seam explicit: the daemon is genuinely a different program
with different concerns (no tty, no user interaction, long-lived).

### Why not async channels for PTY I/O?

`portable-pty` exposes blocking `Read` / `Write`. Bridging to async via
`tokio::task::spawn_blocking` works, but each read/write costs a task
handoff. A dedicated thread reading in a loop has no per-chunk overhead
and is the simpler mental model.

### Why is `ClientInput` broadcast-ignored by other clients?

Today we log `ClientInput` so the log is complete, but we don't broadcast
it. In the multi-attach case, each client only sees the PTY output
(which includes the shell's own echo of what someone typed, if the shell
is configured for that). This keeps the wire simpler but means typing in
one client isn't seen directly by the other — only the results are. We'll
revisit for proper pair-programming UX.

### Why `setsid` over the classic double-fork daemonize?

Double-fork is belt-and-suspenders for "orphan the child completely."
`setsid` alone is enough for our needs: the daemon starts its own session,
detaches from the parent's controlling terminal, and continues running
after the parent exits. We're not writing a traditional daemon that needs
to be immune to `SIGHUP` from a shell logout — we explicitly *want* to
catch `SIGHUP` if someone terminates us, to clean up our state files.

## Roadmap and open questions

See [README.md](README.md#roadmap) for the user-facing roadmap. Here are
a few architecture-level open questions we'd love input on:

- **Log storage abstraction.** Right now `EventLog` is a trait with one
  impl. Does the trait need to be async before we add S3 / sqlite backends,
  or should we leave it sync and put the async I/O in a wrapper?
- **Schema evolution for the log.** If we change the `Event` enum, can we
  still read old logs? Likely needs a per-log version header and
  migration path. Compatible with the serde model but not free.
- **Multi-host replay.** To replay a log recorded on host A on host B, we
  need the log to live somewhere both hosts can see. Filesystem (NFS) is
  easy but slow; object storage is cheap but high-latency. Candidate:
  append to a local log *and* asynchronously upload compressed chunks to
  a shared store.
- **Auth on the socket.** Today `ls -l ~/.local/state/rat/run/*.sock`
  shows `srwxr-xr-x you you`. Fine for local, not fine once the transport
  isn't local. A shared-secret handshake in `Attach` is one option.

If you have opinions on any of these, the
[CONTRIBUTING](CONTRIBUTING.md) guide has instructions for opening an
issue.
