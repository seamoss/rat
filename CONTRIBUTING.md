# Contributing to Rat

Thanks for wanting to contribute. Rat is early — this is the best time to
shape it.

## Table of contents

- [Ways to help](#ways-to-help)
- [Development setup](#development-setup)
- [Project layout](#project-layout)
- [Build, run, test](#build-run-test)
- [Code style](#code-style)
- [Commits and pull requests](#commits-and-pull-requests)
- [Bug reports](#bug-reports)
- [Security](#security)
- [Where help is most wanted](#where-help-is-most-wanted)

## Ways to help

- **Dogfood it.** Run `rat` as your daily multiplexer for a week and open
  issues for anything that bites.
- **Fix bugs.** Anything tagged `good first issue` is a reasonable entry
  point.
- **Build a piece of the roadmap.** See the
  [Roadmap](README.md#roadmap) in the README. Open an issue first for
  anything non-trivial so we can align on approach.
- **Write docs.** Examples, troubleshooting, integration guides, terminal
  emulator compatibility notes — all welcome.
- **Port to new platforms.** See the Windows note in the roadmap.

## Development setup

- **Rust:** 1.80 or newer. The crate uses `edition = "2024"`. Install via
  [rustup](https://rustup.rs).
- **OS:** Linux or macOS. Rat depends on Unix domain sockets and
  `setsid(2)` today; Windows needs a porting pass (see roadmap).
- **Shell:** zsh, bash, or fish for end-to-end testing of the shell
  integration. Any POSIX shell works for basic development.

Clone and build:

```sh
git clone https://github.com/seamoss/rat.git
cd rat
cargo build
```

(SSH: `git clone git@github.com:seamoss/rat.git`)

If you plan to install the debug binaries for local use, symlink them
into a `PATH` directory:

```sh
ln -sf "$PWD/target/debug/rat"        ~/.local/bin/rat
ln -sf "$PWD/target/debug/rat-daemon" ~/.local/bin/rat-daemon
```

The symlinks update automatically on subsequent `cargo build` invocations.
For daily use, swap `debug` for `release` (build with `cargo build --release`).

## Project layout

```
Cargo.toml
src/
├── lib.rs               # module re-exports
├── event.rs             # Event enum + LoggedEvent (seq + timestamp + event)
├── log.rs               # EventLog trait + FileEventLog (JSON-lines append)
├── session.rs           # SessionId, Session, SessionMeta (discoverable state)
├── paths.rs             # canonical paths: state dir, run dir, log, sock, meta
├── protocol.rs          # wire messages + length-prefixed framed JSON codec
└── bin/
    ├── rat.rs           # user-facing CLI: new, attach, list, kill, replay, init
    └── rat-daemon.rs    # background session host (PTY + socket + broadcast)
tests/
└── log_roundtrip.rs     # append → reopen → replay assertions
```

Detailed component notes live in [ARCHITECTURE.md](ARCHITECTURE.md).

## Build, run, test

**Check without building:**

```sh
cargo check
```

**Build both binaries:**

```sh
cargo build             # dev build (fast, unoptimized)
cargo build --release   # release build (slower, optimized)
```

**Run a one-off session from the dev build:**

```sh
cargo run --bin rat -- new -n scratch
```

**Run the tests:**

```sh
cargo test
```

The test suite today is thin (see `tests/log_roundtrip.rs`). Contributions
that expand integration coverage — especially around the wire protocol and
daemon lifecycle — are very welcome.

**Lint:**

```sh
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

Please run both before submitting a PR.

**Debug a daemon that misbehaves:**

```sh
RUST_LOG=debug rat new
# then look at:
cat ~/.local/state/rat/daemon.trace
cat ~/.local/state/rat/daemon.stderr
```

`daemon.trace` records lifecycle events at fine granularity. `daemon.stderr`
captures anything the daemon panics or prints to stderr (most of the time
it's empty).

## Code style

We follow the Rust community defaults. A few project-specific notes:

- **`cargo fmt` is canonical.** Don't argue with the formatter.
- **`cargo clippy` at `-D warnings`.** Don't add `#[allow(...)]` without a
  comment explaining why the lint is wrong here.
- **Comments explain *why*, not *what*.** Well-named identifiers document
  the "what." Reserve comments for hidden constraints, subtle invariants,
  or a workaround for a specific bug. Don't narrate the obvious.
- **Don't design for hypothetical future requirements.** Three similar
  lines is better than a premature abstraction. If we need the abstraction
  later we'll refactor when it's concrete.
- **Errors bubble with `?` + `anyhow::Context`.** Surface the failing
  operation at the boundary (e.g., `.with_context(|| format!("open {path:?}"))`).
- **Avoid holding a `std::sync::Mutex` across `.await`.** It deadlocks
  under the multi-threaded runtime. For cross-await state, use
  `tokio::sync::Mutex`, or scope the lock tightly and drop before awaiting.
- **`unsafe` is allowed for libc FFI** (signals, `setsid`, etc.) where no
  safe alternative exists in our dependencies. Keep the unsafe block as
  small as possible and comment the invariants.

## Commits and pull requests

**Conventional Commits are required.** Releases are automated via
[release-please](https://github.com/googleapis/release-please); the
commit-message *type* determines whether (and how) the next version
bumps, and the subject line is what lands in `CHANGELOG.md`. A commit
that doesn't follow the format won't be rejected, but it also won't
show up in the release notes, so please use one of the types below.

**Format:**

```
<type>(<optional scope>): <imperative subject, 50 chars or less>

<optional body wrapping at 72 chars — explain the *why*>

<optional footers: BREAKING CHANGE: …, Closes #NN, Co-authored-by: …>
```

**Types we use:**

| Type       | Version bump        | Appears in CHANGELOG |
| ---------- | ------------------- | -------------------- |
| `feat`     | Minor (0.x.0)       | Yes — "Features"     |
| `fix`      | Patch (0.0.x)       | Yes — "Bug Fixes"    |
| `perf`     | Patch               | Yes — "Performance"  |
| `revert`   | Patch               | Yes — "Reverts"      |
| `docs`     | None                | Yes — "Documentation"|
| `refactor` | None                | No                   |
| `test`     | None                | No                   |
| `ci`       | None                | No                   |
| `chore`    | None                | No                   |

**Breaking changes** bump major (or minor while we're pre-1.0). Mark
them with either `!` after the type (`feat!: drop RAT_PREFIX fallback`)
or a `BREAKING CHANGE:` footer. Spell out the migration path in the
body — breaking changes are expensive for users, so the more guidance
the better.

**Examples:**

```
feat(daemon): support SIGHUP to cycle the log file

fix(attach): don't deadlock when PTY closes mid-resize

ci: cache cargo registry between runs

docs: add iTerm2 Meta-key configuration walkthrough

feat!: rename rat-daemon to ratd

BREAKING CHANGE: users who installed rat-daemon directly (symlinks,
systemd units, etc.) need to switch to the new name. Automatic upgrade
path is in the README.
```

**Scopes** are optional but helpful when the change is localized —
`keys`, `daemon`, `cli`, `protocol`, `docs`, `ci` are good picks.

**One logical change per commit where practical.** Refactors and
behavior changes in the same commit are painful to review.

**Do not manually edit `Cargo.toml` version, `Cargo.lock`, or
`CHANGELOG.md`.** release-please owns all three — it updates them in
the release PR it maintains on `dev`.

**Pull requests:**

- Rebase onto `dev` before opening; avoid merge commits.
- Include a short description of what changes and why.
- Link related issues with `Closes #NN` / `Fixes #NN`.
- If your PR touches user-facing behavior, update the README accordingly.
- If your PR changes the wire protocol or event-log format, update
  [ARCHITECTURE.md](ARCHITECTURE.md) and call out the compatibility
  implications in the PR description.
- Keep PRs focused. Split scope creep into follow-ups.

**CI:** `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` must
be green. CI runs on every PR against `dev`; you can run the same
checks locally (see [Build, run, test](#build-run-test)).

## Release process

Releases are automated. You shouldn't need to run anything by hand for
a normal release — but here's what happens so the pipeline isn't a
mystery:

1. Conventional commits land on `dev`.
2. The `release-please` workflow maintains an open PR against `dev`
   titled `chore: release <version>`. The PR bumps `Cargo.toml` +
   `Cargo.lock` and regenerates `CHANGELOG.md` from the commits since
   the last tag.
3. When a maintainer merges the release PR, release-please creates a
   `vX.Y.Z` tag and a GitHub Release with the changelog as its body.
4. The `build-release` workflow picks up the `release: published`
   event and uploads cross-compiled tarballs of `rat` + `rat-daemon`
   for x86_64 + aarch64 on Linux and macOS.

If something needs hand-fixing mid-flight, edit the release PR the same
way you'd edit any other PR and re-request review.

## Bug reports

A good bug report includes:

1. **What you did** — exact commands, ideally copy-pasted.
2. **What you expected.**
3. **What happened** — actual output or behavior.
4. **Environment:** OS + version, terminal emulator, shell, `rustc --version`,
   `rat --version` (once we wire that up; for now, commit SHA).
5. **Diagnostics:** contents of `~/.local/state/rat/daemon.trace` and
   `daemon.stderr` are usually the highest-signal thing you can attach.

Tiny repros are magical — if you can reduce the bug to a few lines, do.

## Security

If you find a security issue (session escape, local privilege issue,
anything that could cross a trust boundary), please email the maintainer
privately instead of opening a public issue. We'll coordinate disclosure.

Rat is not designed as a security boundary today — sessions run as the
user who started them, log files are world-readable within that user's
home by default. Treat it like `script(1)` or `tmux(1)`.

## Where help is most wanted

Concrete, scoped contributions with known-good outcomes:

- **Integration tests for the daemon/client pair.** Spawning a daemon in
  a test, attaching a mock client over the Unix socket, asserting on the
  event stream — we have nothing here today.
- **Wire protocol versioning.** Add a protocol version to the Attach
  handshake so we can evolve the format without breaking older clients.
- **Better stale-pid detection on Linux.** `kill(pid, 0)` returns 0 for
  PID-reuse across daemons started at different times. Stashing a start
  timestamp or using `/proc/<pid>/stat`'s `starttime` would make cleanup
  more precise.
- **Replay-time terminal emulation.** Decode the PTY bytes with a VT
  parser (e.g., [`vte`](https://crates.io/crates/vte)) and render a clean
  snapshot at any point — foundation for scrollback search.
- **Window title handling on detach.** We reset the title today, but if
  the user had a custom title before `rat new`, we clobber it. Save and
  restore via OSC 2 / OSC 22.

If you want to work on anything else from the roadmap, open an issue and
say hi first so we can align on approach.
