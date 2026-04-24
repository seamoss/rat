use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self as ct_event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use rat::event::Event;
use rat::paths;
use rat::protocol::{self, ClientMsg, DaemonMsg};
use rat::{EventLog, FileEventLog, SessionId, SessionMeta};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc};

// Detach is a prefix chord rather than a single key because a single key
// (Ctrl-\, etc.) is unreliable once a TUI inside the session enables a
// keyboard-encoding protocol like kitty's CSI-u or xterm's modifyOtherKeys.
// Those protocols re-encode every keypress — including Ctrl-anything — into
// CSI sequences, so a byte-equality scan misses them. A two-step chord
// degrades less often: even if the prefix gets re-encoded by the TUI, the
// follow-up command key still uniquely identifies an intent. Default
// `Ctrl-A` matches `screen`'s long-standing convention; users can override
// via `RAT_PREFIX` (e.g. `C-Space`, `M-a`).
const DEFAULT_PREFIX: Prefix = Prefix::Byte(0x01); // Ctrl-A

// Chord commands (the byte after the prefix).
const CMD_DETACH: u8 = b'd';
const CMD_KILL: u8 = b'D';
const CMD_NEW: u8 = b'c';
const CMD_SWITCH: u8 = b's';
const CMD_HELP: u8 = b'?';
const CMD_COPY: u8 = b'[';

// Default size of the client-side scrollback ring used by copy mode.
// Overridable via RAT_SCROLLBACK_BYTES. Cap matters less than it used
// to because the full transcript also lives in the daemon's log file —
// this buffer is just what copy mode can scroll without hitting disk.
const SCROLLBACK_CAP_DEFAULT: usize = 1_048_576; // 1 MiB

// Meta-prefix flush timeout. A bare ESC has to be delivered to the inner
// app eventually (vim's <Esc> etc.); we wait this long for a follow-up
// byte before giving up and forwarding the ESC. Matches the common
// xterm/vim "esc-timeout" convention.
const META_FLUSH_MS: u64 = 50;

#[derive(Parser)]
#[command(
    name = "rat",
    about = "Rat — cloud-terminal session multiplexer",
    version,
    // Disable clap's default capital -V so we can bind -v ourselves.
    disable_version_flag = true
)]
struct Cli {
    /// Print version and exit.
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
    version: (),
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a new session and attach to it.
    New {
        /// Optional human-friendly name (also exported as $RAT_NAME).
        #[arg(short, long)]
        name: Option<String>,
        /// Skip the nested-session warning and proceed.
        #[arg(short, long)]
        force: bool,
        /// Command to run in the new session; defaults to $SHELL.
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
    /// Attach to an existing session by name or id prefix.
    Attach {
        /// Session name, or UUID / unique UUID prefix.
        id: String,
        /// Skip the nested-session warning and proceed.
        #[arg(short, long)]
        force: bool,
    },
    /// List running sessions.
    List {
        /// Skip the nested-session warning when the picker would attach.
        #[arg(short, long)]
        force: bool,
    },
    /// Replay a session log file non-interactively.
    Replay {
        /// Path to the .log file (under ~/.local/state/rat/).
        path: PathBuf,
    },
    /// Print shell integration to stdout; add `eval "$(rat init <shell>)"` to your rc.
    Init {
        /// zsh | bash | fish
        shell: String,
    },
    /// Kill a session (SIGTERM → SIGKILL on timeout). Prompts for confirmation.
    Kill {
        /// Session name, or UUID / unique UUID prefix.
        id: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
    },
    /// Kill every live session and sweep any stale daemon state. Prompts.
    Killall {
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
    },
    /// Rename a live session (changes the primary name).
    Rename {
        /// Current name, alias, UUID, or UUID prefix.
        id: String,
        /// New primary name. Must be unique among live sessions.
        new_name: String,
    },
    /// Add an alias to a live session (a secondary name that resolves to it).
    Alias {
        /// Session name, alias, UUID, or UUID prefix.
        id: String,
        /// New alias. Must be unique among live sessions.
        alias: String,
    },
    /// Print a shell completion script to stdout. `eval` it, or drop it in
    /// the shell's completion directory for persistent tab-completion.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Search session transcripts for a substring. Works against any .log
    /// file in the state dir, whether or not the daemon is still running.
    Grep {
        /// Substring to search for (case-sensitive).
        pattern: String,
        /// Limit search to a specific session (id prefix / name / alias).
        #[arg(short, long)]
        session: Option<String>,
        /// Search a specific log file instead of all logs in the state dir.
        #[arg(long)]
        log: Option<PathBuf>,
        /// Match raw bytes instead of ANSI-stripped text.
        #[arg(long)]
        raw: bool,
    },
    /// Read-only passive observation of a live session. Replays then
    /// tails the output stream. No input forwarded, no resize sent —
    /// the attached interactive client is untouched. Ctrl-C to stop.
    Watch {
        /// Session name, alias, UUID, or UUID prefix.
        id: String,
    },
    /// Resurrect a dead session: spin up a fresh daemon whose event log
    /// is pre-seeded with the old session's PtyOutput, then attach. The
    /// shell is new (the original PTY is long gone) but the scrollback
    /// is what you left behind.
    Resurrect {
        /// Path to a .log file, or a session name / UUID / UUID prefix
        /// (resolved via the meta file for dead sessions still on disk).
        source: String,
        /// Optional name for the new session.
        #[arg(short, long)]
        name: Option<String>,
        /// Skip the nested-session warning.
        #[arg(short, long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => spawn_bare(false).await,
        Some(Cmd::New { name, cmd, force }) => new(name, cmd, force).await,
        Some(Cmd::Attach { id, force }) => {
            let session_id = resolve_session(&id)?;
            run_attach_cycle(session_id, force).await
        }
        Some(Cmd::List { force }) => {
            if std::io::stdout().is_terminal() {
                list_pick(force).await
            } else {
                list_text()
            }
        }
        Some(Cmd::Replay { path }) => replay(&path),
        Some(Cmd::Init { shell }) => init(&shell),
        Some(Cmd::Kill { id, yes }) => kill(&id, yes),
        Some(Cmd::Killall { yes }) => killall(yes),
        Some(Cmd::Rename { id, new_name }) => rename(&id, &new_name),
        Some(Cmd::Alias { id, alias }) => add_alias(&id, &alias),
        Some(Cmd::Completions { shell }) => completions(shell),
        Some(Cmd::Grep {
            pattern,
            session,
            log,
            raw,
        }) => grep(&pattern, session.as_deref(), log.as_deref(), raw),
        Some(Cmd::Watch { id }) => {
            let session_id = resolve_session(&id)?;
            watch(session_id).await
        }
        Some(Cmd::Resurrect {
            source,
            name,
            force,
        }) => resurrect(&source, name, force).await,
    }
}

fn completions(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}

// `rat grep`: scan PtyOutput events across one or more session logs and
// emit lines that contain the pattern. Output format is
// `{short}:{seq}: {line}` so it composes with normal unix piping.
fn grep(
    pattern: &str,
    session: Option<&str>,
    log: Option<&std::path::Path>,
    raw: bool,
) -> Result<()> {
    let logs: Vec<PathBuf> = if let Some(p) = log {
        vec![p.to_path_buf()]
    } else if let Some(q) = session {
        let meta = read_all_metas()?
            .into_iter()
            .find(|m| meta_has_label(m, q) || m.id.to_string().starts_with(q))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no session matching '{q}' (need a meta file on disk; use --log for raw log paths)"
                )
            })?;
        vec![meta.log_path]
    } else {
        all_log_paths()?
    };

    if logs.is_empty() {
        eprintln!("no logs to search");
        return Ok(());
    }

    let mut total = 0usize;
    for path in logs {
        let log_obj = match FileEventLog::open(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skip {}: {e}", path.display());
                continue;
            }
        };
        let events = log_obj.read_from(0)?;
        let short = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.chars().take(8).collect::<String>())
            .unwrap_or_else(|| "?".into());
        for ev in events {
            if let Event::PtyOutput { data } = &ev.event {
                let text = if raw {
                    String::from_utf8_lossy(data).into_owned()
                } else {
                    strip_ansi(data)
                };
                for line in text.lines() {
                    if line.contains(pattern) {
                        total += 1;
                        println!("{short}:{}: {}", ev.seq, line);
                    }
                }
            }
        }
    }
    if total == 0 {
        eprintln!("no matches for '{pattern}'");
    }
    Ok(())
}

// All .log files under the state dir. Unordered — callers typically just
// iterate and output in discovery order.
fn all_log_paths() -> Result<Vec<PathBuf>> {
    let dir = paths::state_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("log") {
            out.push(path);
        }
    }
    Ok(out)
}

// Quick-and-dirty ANSI stripper for grep output. Drops CSI sequences
// (ESC [ params+intermediates final) and the first byte after any bare
// ESC. OSC sequences (ESC ]) rarely carry grep-relevant text so we
// don't bother — a CSI-only sweep is enough to make `ls`, prompts,
// editor output, etc. readable on the matching side.
fn strip_ansi(data: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b {
            i += 1;
            if i < data.len() && data[i] == b'[' {
                i += 1;
                while i < data.len() && !(0x40..=0x7e).contains(&data[i]) {
                    i += 1;
                }
                if i < data.len() {
                    i += 1;
                }
            } else if i < data.len() {
                i += 1;
            }
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Resurrect: seed a fresh daemon with the PtyOutput from an old log and
// attach. The original PTY process is gone, but replaying its bytes into
// the new session's scrollback reconstructs what the user was looking at
// when it died — which is usually the only thing they miss.
async fn resurrect(source: &str, name: Option<String>, force: bool) -> Result<()> {
    let seed_path = resolve_log_source(source)?;
    warn_if_nested(force)?;
    let session_id = spawn_daemon_with_seed(name, Vec::new(), Some(seed_path)).await?;
    run_attach_cycle(session_id, true).await
}

// Accept either a path to a .log file or a session label/id. For labels
// we look up the meta (works for dead sessions too — the meta file is
// only removed on clean shutdown via cleanup_stale).
fn resolve_log_source(source: &str) -> Result<PathBuf> {
    let path = PathBuf::from(source);
    if path.is_file() {
        return Ok(path);
    }
    let meta = read_all_metas()?
        .into_iter()
        .find(|m| meta_has_label(m, source) || m.id.to_string().starts_with(source))
        .ok_or_else(|| {
            anyhow::anyhow!("no log file or session matching '{source}' (pass a .log path directly if the meta is gone)")
        })?;
    Ok(meta.log_path)
}

fn kill(query: &str, yes: bool) -> Result<()> {
    // Locate metadata by UUID or name. We can't rely on resolve_session alone
    // because it filters out dead sessions — and one use of `kill` is cleaning
    // up stale state from a crashed daemon.
    let metas = read_all_metas()?;
    let matches: Vec<_> = metas
        .iter()
        .filter(|m| meta_has_label(m, query) || m.id.to_string().starts_with(query))
        .cloned()
        .collect();
    let meta = match matches.len() {
        0 => bail!("no session matching '{query}'"),
        1 => matches.into_iter().next().unwrap(),
        n => bail!("'{query}' matches {n} sessions — use more characters"),
    };

    let alive = process_alive(meta.pid);
    if !alive {
        cleanup_stale(&meta);
        println!(
            "session {} was already dead — cleaned up stale state",
            meta.id.short()
        );
        return Ok(());
    }

    // Show what's about to die.
    println!("Kill this session?");
    println!("  id:       {}", meta.id);
    if let Some(n) = &meta.name {
        println!("  name:     {n}");
    }
    println!("  command:  {}", meta.command);
    println!("  pid:      {}", meta.pid);
    println!("  started:  {}", format_started(meta.started));

    if !yes {
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => {}
            _ => {
                println!("aborted");
                return Ok(());
            }
        }
    }

    // SIGTERM first — the daemon will forward it to the PTY child, which
    // triggers the normal shutdown path (SessionEnded, sock/meta cleanup).
    unsafe {
        libc::kill(meta.pid as i32, libc::SIGTERM);
    }

    // Wait up to 2s for the daemon to exit cleanly.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_alive(meta.pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Escalate if needed.
    if process_alive(meta.pid) {
        eprintln!("SIGTERM ignored after 2s — escalating to SIGKILL");
        unsafe {
            libc::kill(meta.pid as i32, libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    cleanup_stale(&meta);
    println!("killed session {}", meta.id.short());
    Ok(())
}

fn killall(yes: bool) -> Result<()> {
    let metas = read_all_metas()?;
    if metas.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    let (live, dead): (Vec<SessionMeta>, Vec<SessionMeta>) =
        metas.into_iter().partition(|m| process_alive(m.pid));

    if live.is_empty() {
        // Nothing to kill — but sweep any orphaned state files.
        for m in &dead {
            cleanup_stale(m);
        }
        println!("no live sessions (swept {} stale)", dead.len());
        return Ok(());
    }

    println!(
        "About to kill {} live session(s){}:",
        live.len(),
        if dead.is_empty() {
            String::new()
        } else {
            format!(" (+ sweep {} stale)", dead.len())
        }
    );
    for m in &live {
        let label = label_column(m);
        println!(
            "  {:<10} {:<20} pid {:<6} {}",
            m.id.short(),
            truncate(&label, 20),
            m.pid,
            m.command
        );
    }
    // Flag the footgun: killing all includes the ambient session.
    if let Ok(cur) = std::env::var("RAT_SESSION")
        && !cur.is_empty()
        && live.iter().any(|m| m.id.to_string() == cur)
    {
        let short = &cur[..cur.len().min(8)];
        println!("NOTE: you are currently inside session {short} — killing will drop you.");
    }

    if !yes {
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }

    // SIGTERM every live daemon in one pass so they shut down concurrently,
    // then wait once for up to 2s. This is an order of magnitude faster than
    // looping the per-session kill() path when there are many sessions.
    for m in &live {
        unsafe {
            libc::kill(m.pid as i32, libc::SIGTERM);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if live.iter().all(|m| !process_alive(m.pid)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut hard_killed = 0;
    for m in &live {
        if process_alive(m.pid) {
            unsafe {
                libc::kill(m.pid as i32, libc::SIGKILL);
            }
            hard_killed += 1;
        }
    }
    if hard_killed > 0 {
        eprintln!("{hard_killed} daemon(s) ignored SIGTERM — escalated to SIGKILL");
        std::thread::sleep(Duration::from_millis(100));
    }

    for m in live.iter().chain(dead.iter()) {
        cleanup_stale(m);
    }

    println!(
        "killed {} session(s){}",
        live.len(),
        if dead.is_empty() {
            String::new()
        } else {
            format!(", swept {} stale", dead.len())
        }
    );
    Ok(())
}

fn rename(query: &str, new_name: &str) -> Result<()> {
    let new_name = validate_label(new_name)?;
    let mut meta = find_live_meta(query)?;
    if meta.name.as_deref() == Some(new_name) {
        bail!("session '{query}' is already named '{new_name}'");
    }
    ensure_label_free(new_name, Some(meta.id))?;

    let old = meta
        .name
        .clone()
        .unwrap_or_else(|| meta.id.short().to_string());
    meta.name = Some(new_name.to_string());
    write_meta(&meta)?;
    // $RAT_NAME inside the running shell was exported at PTY spawn and can't
    // be mutated from outside — the prompt decoration will keep showing the
    // old name until the shell is restarted.
    println!(
        "renamed {} '{}' → '{}'  (note: $RAT_NAME in the running shell is unchanged)",
        meta.id.short(),
        old,
        new_name
    );
    Ok(())
}

fn add_alias(query: &str, alias: &str) -> Result<()> {
    let alias = validate_label(alias)?;
    let mut meta = find_live_meta(query)?;
    ensure_label_free(alias, Some(meta.id))?;
    if meta.aliases.iter().any(|a| a == alias) {
        bail!("session {} already has alias '{alias}'", meta.id.short());
    }
    meta.aliases.push(alias.to_string());
    write_meta(&meta)?;
    println!("aliased {} → '{alias}'", meta.id.short());
    Ok(())
}

// Resolve a label or UUID to a live session's meta, or error out. Unlike
// resolve_session this returns the whole meta (so callers can mutate it),
// and it explicitly rejects dead sessions since rename/alias shouldn't
// touch stale on-disk state.
fn find_live_meta(query: &str) -> Result<SessionMeta> {
    let mut matches: Vec<SessionMeta> = read_all_metas()?
        .into_iter()
        .filter(|m| process_alive(m.pid))
        .filter(|m| meta_has_label(m, query) || m.id.to_string().starts_with(query))
        .collect();
    match matches.len() {
        0 => bail!("no live session matching '{query}'"),
        1 => Ok(matches.remove(0)),
        n => bail!("'{query}' matches {n} sessions — use more characters or a unique name"),
    }
}

fn ensure_label_free(label: &str, allow_same: Option<SessionId>) -> Result<()> {
    for m in read_all_metas()? {
        if !process_alive(m.pid) {
            continue;
        }
        if Some(m.id) == allow_same {
            continue;
        }
        if meta_has_label(&m, label) {
            bail!(
                "label '{label}' is already taken by session {}",
                m.id.short()
            );
        }
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<&str> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        bail!("label must not be empty");
    }
    // Keep labels out of the namespace that resolve_session treats as a UUID
    // prefix — otherwise `rat attach <alias>` could be ambiguous with a real
    // UUID substring.
    if trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && trimmed.len() >= 4 {
        bail!("label '{trimmed}' looks like a UUID prefix — pick something more distinctive");
    }
    Ok(trimmed)
}

fn write_meta(meta: &SessionMeta) -> Result<()> {
    let path = paths::meta_path(&meta.id.to_string());
    std::fs::write(&path, serde_json::to_vec_pretty(meta)?)
        .with_context(|| format!("rewrite {}", path.display()))?;
    Ok(())
}

fn cleanup_stale(meta: &SessionMeta) {
    let id = meta.id.to_string();
    let _ = std::fs::remove_file(paths::sock_path(&id));
    let _ = std::fs::remove_file(paths::meta_path(&id));
}

fn init(shell: &str) -> Result<()> {
    let snippet = match shell {
        "zsh" => ZSH_INIT,
        "bash" => BASH_INIT,
        "fish" => FISH_INIT,
        other => bail!("unsupported shell '{other}' (expected zsh, bash, or fish)"),
    };
    print!("{snippet}");
    Ok(())
}

const ZSH_INIT: &str = r#"# rat shell integration
if [[ -n "${RAT_SESSION:-}" ]]; then
  if [[ -n "${RAT_NAME:-}" ]]; then
    _rat_tag="%F{208}[rat:${RAT_NAME}]%f"
  else
    _rat_tag="%F{208}[rat]%f"
  fi
  PROMPT="${_rat_tag} ${PROMPT}"
  unset _rat_tag
fi
"#;

const BASH_INIT: &str = r#"# rat shell integration
if [[ -n "${RAT_SESSION:-}" ]]; then
  if [[ -n "${RAT_NAME:-}" ]]; then
    _rat_tag="\[\033[38;5;208m\][rat:${RAT_NAME}]\[\033[0m\]"
  else
    _rat_tag="\[\033[38;5;208m\][rat]\[\033[0m\]"
  fi
  PS1="${_rat_tag} ${PS1}"
  unset _rat_tag
fi
"#;

const FISH_INIT: &str = r#"# rat shell integration
if set -q RAT_SESSION
  if not functions -q __rat_prompt_original
    functions --copy fish_prompt __rat_prompt_original
  end
  function fish_prompt
    set_color FFA500
    if set -q RAT_NAME
      echo -n "[rat:$RAT_NAME] "
    else
      echo -n "[rat] "
    end
    set_color normal
    __rat_prompt_original
  end
end
"#;

async fn new(name: Option<String>, cmd: Vec<String>, force: bool) -> Result<()> {
    warn_if_nested(force)?;
    let session_id = spawn_daemon(name, cmd).await?;
    // Already passed the nested-session check above; don't prompt again.
    run_attach_cycle(session_id, true).await
}

// Bare `rat` with no subcommand: spawn a detached daemon, print its id,
// don't attach. Useful for scripts and "I want a session to pick up later."
async fn spawn_bare(force: bool) -> Result<()> {
    warn_if_nested(force)?;
    let session_id = spawn_daemon(None, Vec::new()).await?;
    println!("{session_id}");
    Ok(())
}

// Fork rat-daemon into its own session and wait until it's bound the
// Unix socket. Returns the session id on success. `seed_log`, when set,
// is forwarded as the daemon's `--seed-log` flag (see `rat resurrect`).
async fn spawn_daemon(name: Option<String>, cmd: Vec<String>) -> Result<SessionId> {
    spawn_daemon_with_seed(name, cmd, None).await
}

async fn spawn_daemon_with_seed(
    name: Option<String>,
    cmd: Vec<String>,
    seed_log: Option<PathBuf>,
) -> Result<SessionId> {
    if let Some(n) = &name {
        if n.is_empty() {
            bail!("--name must not be empty");
        }
        if let Some(existing) = find_live_by_name(n)? {
            bail!(
                "session named '{n}' is already running ({}). Attach with: rat attach {n}",
                existing.id.short()
            );
        }
    }

    let session_id = SessionId::new();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    std::fs::create_dir_all(paths::state_dir())?;
    std::fs::create_dir_all(paths::run_dir())?;

    let self_exe = std::env::current_exe().context("current_exe")?;
    let daemon_exe = self_exe.with_file_name("rat-daemon");
    if !daemon_exe.exists() {
        bail!("rat-daemon not found at {}", daemon_exe.display());
    }

    let daemon_stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::state_dir().join("daemon.stdout"))?;
    let daemon_stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::state_dir().join("daemon.stderr"))?;

    let mut builder = std::process::Command::new(&daemon_exe);
    builder
        .arg("--id")
        .arg(session_id.to_string())
        .arg("--cols")
        .arg(cols.to_string())
        .arg("--rows")
        .arg(rows.to_string());
    if let Some(n) = &name {
        builder.arg("--name").arg(n);
    }
    if let Some(seed) = &seed_log {
        builder.arg("--seed-log").arg(seed);
    }
    if !cmd.is_empty() {
        builder.arg("--");
        for a in &cmd {
            builder.arg(a);
        }
    }
    builder
        .stdin(Stdio::null())
        .stdout(daemon_stdout)
        .stderr(daemon_stderr);

    // Detach from our session/controlling tty so closing our terminal
    // doesn't take the daemon down.
    unsafe {
        builder.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    builder.spawn().context("spawn rat-daemon")?;

    // Wait up to 3s for the daemon to bind its socket.
    let sock = paths::sock_path(&session_id.to_string());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    if !sock.exists() {
        bail!(
            "daemon didn't bind socket within 3s — see {}",
            paths::state_dir().join("daemon.stderr").display()
        );
    }

    Ok(session_id)
}

// What the client should do once the current attach has ended. Set by the
// chord filter when the user presses a chord that's "detach then <thing>",
// so the main loop can re-enter attach against a different session instead
// of returning all the way to the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PostAttach {
    None,
    Switch,
    NewSession,
    Kill,
}

impl PostAttach {
    fn from_chord(a: ChordAction) -> Self {
        match a {
            ChordAction::None | ChordAction::Detach => Self::None,
            ChordAction::DetachThenSwitch => Self::Switch,
            ChordAction::DetachThenNew => Self::NewSession,
            ChordAction::DetachThenKill => Self::Kill,
        }
    }
}

async fn attach(session_id: SessionId, force: bool) -> Result<PostAttach> {
    warn_if_nested(force)?;
    // Resolve the detach prefix before we enter raw mode — a bad RAT_PREFIX
    // should surface as a normal error, not a stuck terminal.
    let prefix = resolve_prefix()?;

    let sock = paths::sock_path(&session_id.to_string());
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connect {}", sock.display()))?;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let w = Arc::new(tokio::sync::Mutex::new(w));

    protocol::write_frame(&mut *w.lock().await, &ClientMsg::Attach { from_seq: 0 }).await?;

    let first: DaemonMsg = protocol::read_frame(&mut r).await?;
    let (sid, sname) = match first {
        DaemonMsg::Attached {
            session_id, name, ..
        } => (session_id, name),
        DaemonMsg::Error { message } => bail!("daemon error: {message}"),
        _ => bail!("expected Attached first"),
    };

    print_attach_banner(&sid, sname.as_deref(), prefix);

    // Resize to match this client's terminal (in case we reattached from a
    // different-sized terminal than the one that started the session).
    protocol::write_frame(&mut *w.lock().await, &ClientMsg::Resize { cols, rows }).await?;

    let _raw = RawModeGuard::enable()?;

    let done = Arc::new(Notify::new());
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exit_code = Arc::new(std::sync::Mutex::new(None::<i32>));

    // Copy-mode shared state. The scrollback ring is appended to on every
    // filtered PtyOutput chunk; when copy mode is active the output task
    // diverts new bytes into `pending` instead of writing to stdout, and
    // `run_copy_mode` flushes `pending` on exit so the live view catches
    // up with whatever the shell produced while the user was scrolling.
    let scrollback_cap = scrollback_cap();
    let scrollback = Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<u8>::with_capacity(scrollback_cap.min(4096)),
    ));
    let in_copy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pending = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

    // output_task: daemon → stdout (or → pending while in copy mode)
    let done_o = Arc::clone(&done);
    let ended_o = Arc::clone(&ended);
    let exit_code_o = Arc::clone(&exit_code);
    let scrollback_o = Arc::clone(&scrollback);
    let in_copy_o = Arc::clone(&in_copy);
    let pending_o = Arc::clone(&pending);
    let passthrough_kbd = kbd_passthrough_enabled();
    let output_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut filter = OutputFilter::new(passthrough_kbd);
        loop {
            let msg: DaemonMsg = match protocol::read_frame(&mut r).await {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                DaemonMsg::Event { event } => match event.event {
                    Event::PtyOutput { data } => {
                        let filtered = filter.process(&data);
                        if filtered.is_empty() {
                            continue;
                        }
                        append_scrollback(&scrollback_o, &filtered, scrollback_cap);
                        if in_copy_o.load(std::sync::atomic::Ordering::SeqCst) {
                            pending_o.lock().unwrap().extend_from_slice(&filtered);
                        } else {
                            if stdout.write_all(&filtered).await.is_err() {
                                break;
                            }
                            let _ = stdout.flush().await;
                        }
                    }
                    Event::SessionEnded { exit_code: code } => {
                        ended_o.store(true, std::sync::atomic::Ordering::SeqCst);
                        *exit_code_o.lock().unwrap() = code;
                        break;
                    }
                    _ => {}
                },
                DaemonMsg::SessionEnded { exit_code: code } => {
                    ended_o.store(true, std::sync::atomic::Ordering::SeqCst);
                    *exit_code_o.lock().unwrap() = code;
                    break;
                }
                _ => {}
            }
        }
        done_o.notify_waiters();
    });

    // stdin reader thread → channel (raw bytes).
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("stdin-reader".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                let n = match stdin.lock().read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                if stdin_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        })?;

    // input_task: channel → daemon Input, intercepting the chord. A shared
    // slot carries the post-detach action back to the main loop so we can
    // re-attach elsewhere (switch), spawn a new session (c), or kill (D).
    let post = Arc::new(std::sync::Mutex::new(PostAttach::None));
    let post_i = Arc::clone(&post);
    let w_i = Arc::clone(&w);
    let done_i = Arc::clone(&done);
    let scrollback_i = Arc::clone(&scrollback);
    let in_copy_i = Arc::clone(&in_copy);
    let pending_i = Arc::clone(&pending);
    let input_task = tokio::spawn(async move {
        let mut filter = InputFilter::new(prefix);
        loop {
            // When the filter is mid-ESC (Meta-prefix path) we race the
            // stdin recv against a short timeout so a bare ESC keypress
            // isn't held indefinitely — vim etc. needs the ESC delivered
            // within ~50ms of the user pressing it.
            let data = if filter.awaiting_esc() {
                tokio::select! {
                    maybe = stdin_rx.recv() => match maybe {
                        Some(d) => d,
                        None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(META_FLUSH_MS)) => {
                        let flushed = filter.flush();
                        if !flushed.is_empty()
                            && protocol::write_frame(
                                &mut *w_i.lock().await,
                                &ClientMsg::Input { data: flushed },
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                }
            } else {
                match stdin_rx.recv().await {
                    Some(d) => d,
                    None => break,
                }
            };

            let action = filter.process(&data);
            if action.help {
                print_chord_help_inline();
            }
            if action.copy_mode {
                run_copy_mode(&scrollback_i, &in_copy_i, &pending_i, &mut stdin_rx).await;
            }
            if !action.forward.is_empty()
                && protocol::write_frame(
                    &mut *w_i.lock().await,
                    &ClientMsg::Input {
                        data: action.forward,
                    },
                )
                .await
                .is_err()
            {
                break;
            }
            if action.action.detaches() {
                let _ = protocol::write_frame(&mut *w_i.lock().await, &ClientMsg::Detach).await;
                *post_i.lock().unwrap() = PostAttach::from_chord(action.action);
                break;
            }
        }
        done_i.notify_waiters();
    });

    // resize_task: SIGWINCH → Resize
    let w_r = Arc::clone(&w);
    let done_r = Arc::clone(&done);
    let resize_task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sig = match signal(SignalKind::window_change()) {
            Ok(s) => s,
            Err(_) => return,
        };
        loop {
            tokio::select! {
                _ = sig.recv() => {
                    if let Ok((cols, rows)) = terminal::size() {
                        let _ = protocol::write_frame(&mut *w_r.lock().await, &ClientMsg::Resize { cols, rows }).await;
                    }
                }
                _ = done_r.notified() => break,
            }
        }
    });

    done.notified().await;

    output_task.abort();
    input_task.abort();
    resize_task.abort();

    drop(_raw);

    let post = *post.lock().unwrap();
    if ended.load(std::sync::atomic::Ordering::SeqCst) {
        print_session_ended(&sid, sname.as_deref(), *exit_code.lock().unwrap());
    } else if matches!(post, PostAttach::None) {
        // Suppress the "detached" banner when we're about to chain into
        // another attach / action — the next banner will take its place.
        print_detach_banner(&sid, sname.as_deref());
    }
    Ok(post)
}

// Drives a series of attach() calls, chaining on the PostAttach action
// returned by each one. A single `rat attach` invocation can thus walk
// between sessions (via `<prefix> s`) or spawn-and-attach a fresh one
// (via `<prefix> c`) without the user having to re-enter `rat`.
async fn run_attach_cycle(mut id: SessionId, force: bool) -> Result<()> {
    let mut first = true;
    loop {
        let post = attach(id, if first { force } else { true }).await?;
        first = false;
        match post {
            PostAttach::None => return Ok(()),
            PostAttach::Switch => match pick_live_session()? {
                Some(next) => {
                    id = next;
                }
                None => return Ok(()),
            },
            PostAttach::NewSession => {
                let new_id = spawn_daemon(None, Vec::new()).await?;
                id = new_id;
            }
            PostAttach::Kill => {
                kill_after_detach(id)?;
                return Ok(());
            }
        }
    }
}

// Read-only attach. Connects the same way a normal client does, replays
// the log from seq 0, and streams PtyOutput to stdout. Never sends
// ClientMsg::Input, ClientMsg::Detach, or ClientMsg::Resize, so the
// interactive attacher (if any) is unaffected. Stays in cooked mode so
// Ctrl-C from the terminal line discipline cleanly ends the watch.
async fn watch(session_id: SessionId) -> Result<()> {
    let sock = paths::sock_path(&session_id.to_string());
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connect {}", sock.display()))?;

    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut w = w;

    protocol::write_frame(&mut w, &ClientMsg::Attach { from_seq: 0 }).await?;

    let first: DaemonMsg = protocol::read_frame(&mut r).await?;
    let (sid, sname) = match first {
        DaemonMsg::Attached {
            session_id, name, ..
        } => (session_id, name),
        DaemonMsg::Error { message } => bail!("daemon error: {message}"),
        _ => bail!("expected Attached first"),
    };

    print_watch_banner(&sid, sname.as_deref());

    let stop = Arc::new(Notify::new());
    let stop_c = Arc::clone(&stop);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop_c.notify_one();
    });

    let passthrough_kbd = kbd_passthrough_enabled();
    let mut filter = OutputFilter::new(passthrough_kbd);
    let mut stdout = tokio::io::stdout();
    let mut ended_code: Option<Option<i32>> = None;

    loop {
        tokio::select! {
            _ = stop.notified() => break,
            msg = protocol::read_frame::<_, DaemonMsg>(&mut r) => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    DaemonMsg::Event { event } => match event.event {
                        Event::PtyOutput { data } => {
                            let filtered = filter.process(&data);
                            if !filtered.is_empty()
                                && stdout.write_all(&filtered).await.is_err()
                            {
                                break;
                            }
                            let _ = stdout.flush().await;
                        }
                        Event::SessionEnded { exit_code: code } => {
                            ended_code = Some(code);
                            break;
                        }
                        _ => {}
                    },
                    DaemonMsg::SessionEnded { exit_code: code } => {
                        ended_code = Some(code);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(code) = ended_code {
        print_session_ended(&sid, sname.as_deref(), code);
    } else {
        eprintln!();
        eprintln!("— watch stopped for {} —", sname.as_deref().unwrap_or(&sid));
    }
    Ok(())
}

fn print_watch_banner(session_id: &str, name: Option<&str>) {
    let short = &session_id[..session_id.len().min(8)];
    eprint!("\x1b]2;[rat watch] {short}\x07");
    eprintln!();
    eprintln!("{}", frame_top());
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {BOLD}rat{RESET} {DIM}·{RESET} watching {DIM}(read-only){RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   session:  {BOLD}{session_id}{RESET}{}",
            name.map(|n| format!(" {DIM}({n}){RESET}"))
                .unwrap_or_default()
        ))
    );
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}(Ctrl-C to stop · no input forwarded to the session){RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!("{}", frame_bot());
    eprintln!();
}

fn list_text() -> Result<()> {
    let mut metas = read_all_metas()?;
    if metas.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    metas.sort_by_key(|m| m.started);
    println!(
        "{:<10} {:<20} {:<8} {:<10} {:<14} COMMAND",
        "ID", "NAME", "PID", "STATE", "STARTED"
    );
    for m in metas {
        let alive = process_alive(m.pid);
        let state = if alive { "running" } else { "dead" };
        let label = label_column(&m);
        println!(
            "{:<10} {:<20} {:<8} {:<10} {:<14} {}",
            m.id.short(),
            truncate(&label, 20),
            m.pid,
            state,
            format_started(m.started),
            m.command
        );
    }
    Ok(())
}

// Primary name plus aliases, collapsed into one cell: `agent,ag,a` or
// just the name, or `-` if the session has neither.
fn label_column(m: &SessionMeta) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(n) = &m.name {
        parts.push(n);
    }
    for a in &m.aliases {
        parts.push(a);
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(",")
    }
}

async fn list_pick(force: bool) -> Result<()> {
    let mut alive: Vec<SessionMeta> = read_all_metas()?
        .into_iter()
        .filter(|m| process_alive(m.pid))
        .collect();
    if alive.is_empty() {
        println!("no running sessions");
        return Ok(());
    }
    alive.sort_by_key(|m| m.started);

    // Prompt before opening the picker — an alt-screen picker flickering up
    // only to bail on the inner attach would be worse UX.
    warn_if_nested(force)?;

    // Separate the raw-mode/alt-screen scope from the await below: we MUST
    // restore the terminal before calling attach(), otherwise attach()'s own
    // raw-mode setup collides with our own.
    let selected = run_picker(&alive)?;

    if let Some(id) = selected {
        // Already confirmed — don't prompt again from inside attach().
        run_attach_cycle(id, true).await?;
    }
    Ok(())
}

// Picker-only variant of `list_pick`: no banner prompt, no attach. Used
// by the `<prefix> s` chord to let the user hop between live sessions
// without leaving the attach cycle.
fn pick_live_session() -> Result<Option<SessionId>> {
    let mut alive: Vec<SessionMeta> = read_all_metas()?
        .into_iter()
        .filter(|m| process_alive(m.pid))
        .collect();
    if alive.is_empty() {
        eprintln!("no running sessions");
        return Ok(None);
    }
    alive.sort_by_key(|m| m.started);
    run_picker(&alive)
}

fn run_picker(items: &[SessionMeta]) -> Result<Option<SessionId>> {
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    terminal::enable_raw_mode()?;

    // Always tear the terminal back down even on error.
    let result = (|| -> Result<Option<SessionId>> {
        let mut cursor = 0usize;
        loop {
            draw_picker(&mut stdout, items, cursor)?;
            let ev = ct_event::read()?;
            if let ct_event::Event::Key(k) = ev {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        cursor = cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if cursor + 1 < items.len() => {
                        cursor += 1;
                    }
                    KeyCode::Home => cursor = 0,
                    KeyCode::End => cursor = items.len().saturating_sub(1),
                    KeyCode::Enter => return Ok(Some(items[cursor].id)),
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    _ => {}
                }
            }
        }
    })();

    let _ = terminal::disable_raw_mode();
    let _ = execute!(stdout, Show, LeaveAlternateScreen);
    result
}

fn draw_picker(stdout: &mut std::io::Stdout, items: &[SessionMeta], cursor: usize) -> Result<()> {
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    writeln!(
        stdout,
        "{ORANGE}rat · select session{RESET}   {DIM}↑↓ move · enter attach · q/esc cancel{RESET}\r"
    )?;
    writeln!(stdout, "\r")?;
    writeln!(
        stdout,
        "  {DIM}  {:<10} {:<20} {:<14} COMMAND{RESET}\r",
        "ID", "NAME", "STARTED"
    )?;
    for (i, m) in items.iter().enumerate() {
        let label = label_column(m);
        let row = format!(
            "{:<10} {:<20} {:<14} {}",
            m.id.short(),
            truncate(&label, 20),
            format_started(m.started),
            m.command
        );
        if i == cursor {
            writeln!(stdout, "  {ORANGE}▸ {BOLD}{row}{RESET}\r")?;
        } else {
            writeln!(stdout, "    {row}\r")?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn read_all_metas() -> Result<Vec<SessionMeta>> {
    let run = paths::run_dir();
    if !run.exists() {
        return Ok(Vec::new());
    }
    let mut metas = Vec::new();
    for entry in std::fs::read_dir(&run)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".meta.json") {
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(m) = serde_json::from_slice::<SessionMeta>(&bytes) {
            metas.push(m);
        }
    }
    Ok(metas)
}

fn find_live_by_name(name: &str) -> Result<Option<SessionMeta>> {
    for m in read_all_metas()? {
        if process_alive(m.pid) && meta_has_label(&m, name) {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

fn meta_has_label(meta: &SessionMeta, label: &str) -> bool {
    meta.name.as_deref() == Some(label) || meta.aliases.iter().any(|a| a == label)
}

fn replay(log_path: &PathBuf) -> Result<()> {
    let log = FileEventLog::open(log_path)?;
    let events = log.read_from(0)?;
    let mut stdout = std::io::stdout().lock();
    for ev in events {
        if let Event::PtyOutput { data } = ev.event {
            stdout.write_all(&data)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn resolve_session(query: &str) -> Result<SessionId> {
    // Exact UUID parse always wins.
    if let Ok(id) = query.parse::<SessionId>() {
        return Ok(id);
    }
    // Otherwise match against live sessions by label (name or alias, exact)
    // or UUID prefix. Dead-pid metas are ignored — you can't attach to a dead
    // daemon anyway.
    let mut by_name = Vec::new();
    let mut by_prefix = Vec::new();
    for m in read_all_metas()? {
        if !process_alive(m.pid) {
            continue;
        }
        if meta_has_label(&m, query) {
            by_name.push(m.id);
        } else if m.id.to_string().starts_with(query) {
            by_prefix.push(m.id);
        }
    }
    // Exact label match takes precedence.
    if !by_name.is_empty() {
        if by_name.len() == 1 {
            return Ok(by_name[0]);
        }
        bail!("name '{query}' matches {} live sessions", by_name.len());
    }
    match by_prefix.len() {
        0 => bail!("no live session matching '{query}'"),
        1 => Ok(by_prefix[0]),
        n => bail!("'{query}' matches {n} sessions — use more characters or a name"),
    }
}

fn process_alive(pid: u32) -> bool {
    // kill(pid, 0) returns 0 if the process exists and we have perms to signal it.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn format_started(t: SystemTime) -> String {
    match t.elapsed() {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        Err(_) => "?".into(),
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode().context("enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

// --- banner frame helpers ------------------------------------------------
//
// Frames have a fixed internal width. `frame_row` takes content that may
// contain ANSI color codes and pads to that width based on *visible* length
// (codes don't count toward visible cells). All banners share these helpers
// so they look like one family.

const ORANGE: &str = "\x1b[38;5;208m";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const FRAME_W: usize = 60;

fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip through CSI sequence (ends at an ASCII alphabetic byte).
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

fn frame_top() -> String {
    format!("{ORANGE}╭{}╮{RESET}", "─".repeat(FRAME_W))
}
fn frame_bot() -> String {
    format!("{ORANGE}╰{}╯{RESET}", "─".repeat(FRAME_W))
}
fn frame_blank() -> String {
    format!("{ORANGE}│{RESET}{}{ORANGE}│{RESET}", " ".repeat(FRAME_W))
}
fn frame_row(content: &str) -> String {
    let vlen = visible_len(content);
    let pad = FRAME_W.saturating_sub(vlen);
    format!(
        "{ORANGE}│{RESET}{content}{}{ORANGE}│{RESET}",
        " ".repeat(pad)
    )
}

fn print_attach_banner(session_id: &str, name: Option<&str>, prefix: Prefix) {
    let short = &session_id[..session_id.len().min(8)];
    // OSC 2: set terminal title.
    eprint!("\x1b]2;[rat] {short}\x07");

    let version = env!("CARGO_PKG_VERSION");
    let pd = prefix_display(prefix);
    eprintln!();
    eprintln!("{}", frame_top());
    eprintln!("{}", frame_blank());
    eprintln!("{}", frame_row(&format!("   {ORANGE}█▀█ ▄▀█ ▀█▀{RESET}")));
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {ORANGE}█▀▄ █▀█ ░█░{RESET}   {BOLD}rat{RESET} {DIM}v{version} · cloud-terminal multiplexer{RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!("   session:  {BOLD}{session_id}{RESET}"))
    );
    if let Some(n) = name {
        eprintln!("{}", frame_row(&format!("   name:     {BOLD}{n}{RESET}")));
    }
    eprintln!("{}", frame_row(&format!("   prefix:   {BOLD}{pd}{RESET}")));
    eprintln!(
        "{}",
        frame_row(&format!(
            "   chord:    {BOLD}d{RESET}=detach {BOLD}c{RESET}=new {BOLD}s{RESET}=switch {BOLD}D{RESET}=kill {BOLD}[{RESET}=copy {BOLD}?{RESET}=help"
        ))
    );
    // Literal-prefix echo only exists for byte prefixes.
    let literal_hint = match prefix {
        Prefix::Byte(_) => format!("{pd} {pd} for literal · "),
        Prefix::Meta(_) => String::new(),
    };
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}({literal_hint}Ctrl-D / exit ends session){RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!("{}", frame_bot());
    eprintln!();
}

fn print_detach_banner(session_id: &str, name: Option<&str>) {
    eprint!("\x1b]2;\x07");
    let short = &session_id[..session_id.len().min(8)];
    let reattach_target = name.unwrap_or(short);

    eprintln!();
    eprintln!("{}", frame_top());
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!("   {BOLD}rat{RESET} {DIM}·{RESET} detached"))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   still running:  {BOLD}{short}{RESET}{}",
            name.map(|n| format!(" {DIM}({n}){RESET}"))
                .unwrap_or_default()
        ))
    );
    eprintln!(
        "{}",
        frame_row(&format!(
            "   reattach:       {DIM}rat attach {reattach_target}{RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!("{}", frame_bot());
    eprintln!();
}

fn print_session_ended(session_id: &str, name: Option<&str>, exit_code: Option<i32>) {
    eprint!("\x1b]2;\x07");
    let short = &session_id[..session_id.len().min(8)];

    eprintln!();
    eprintln!("{}", frame_top());
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!("   {BOLD}rat{RESET} {DIM}·{RESET} session ended"))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   session:    {BOLD}{short}{RESET}{}",
            name.map(|n| format!(" {DIM}({n}){RESET}"))
                .unwrap_or_default()
        ))
    );
    let exit_str = match exit_code {
        Some(c) => format!("{c}"),
        None => "(signal)".into(),
    };
    eprintln!(
        "{}",
        frame_row(&format!("   exit code:  {BOLD}{exit_str}{RESET}"))
    );
    eprintln!("{}", frame_blank());
    eprintln!("{}", frame_bot());
    eprintln!();
}

// --- nested-session guard ------------------------------------------------

fn warn_if_nested(force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let outer = match std::env::var("RAT_SESSION") {
        Ok(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    let outer_name = std::env::var("RAT_NAME").ok().filter(|n| !n.is_empty());
    let short = &outer[..outer.len().min(8)];
    let label = outer_name.as_deref().unwrap_or(short);

    eprintln!();
    eprintln!("{}", frame_top());
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {ORANGE}{BOLD}⚠  nested rat session detected{RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!("   you are inside:  {BOLD}{label}{RESET}"))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}Running rat inside rat stacks raw-mode clients,{RESET}"
        ))
    );
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}routes the detach chord to the outermost client,{RESET}"
        ))
    );
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}and generally leads to confusing input behaviour.{RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}Detach first ({BOLD}<prefix> d{RESET}{DIM}) unless you know{RESET}"
        ))
    );
    eprintln!(
        "{}",
        frame_row(&format!(
            "   {DIM}you want this. Pass {BOLD}--force{RESET}{DIM} to skip this prompt.{RESET}"
        ))
    );
    eprintln!("{}", frame_blank());
    eprintln!("{}", frame_bot());
    eprintln!();

    eprint!("Continue anyway? [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => bail!("aborted — already inside rat session '{label}'"),
    }
}

// Post-detach handler for `<prefix> D`. Prompts (y/N) because capital-D
// is one Shift-slip away from plain `d` and we don't want accidental
// session kills. Reuses the SIGTERM → 2s wait → SIGKILL → cleanup dance
// from the normal `rat kill` path.
fn kill_after_detach(id: SessionId) -> Result<()> {
    let meta = read_all_metas()?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow::anyhow!("no meta for session {}", id.short()))?;

    print!("Kill session {}? [y/N] ", id.short());
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        println!("aborted");
        return Ok(());
    }

    if !process_alive(meta.pid) {
        cleanup_stale(&meta);
        println!("session {} was already dead — cleaned up", id.short());
        return Ok(());
    }

    unsafe {
        libc::kill(meta.pid as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_alive(meta.pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if process_alive(meta.pid) {
        eprintln!("SIGTERM ignored after 2s — escalating to SIGKILL");
        unsafe {
            libc::kill(meta.pid as i32, libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cleanup_stale(&meta);
    println!("killed session {}", id.short());
    Ok(())
}

// Printed in raw mode when the user presses `<prefix> ?`. The leading
// `\r\n` puts the line on its own row regardless of where the inner app
// left the cursor; the trailing `\r\n` leaves the cursor at column 1 so
// subsequent PTY output doesn't overlap. Inner apps may still need a
// Ctrl-L to fully repaint — the trade-off for zero cursor-save gymnastics.
fn print_chord_help_inline() {
    eprint!(
        "\r\n{ORANGE}▸ rat chord{RESET}  \
         d=detach  c=new  s=switch  D=kill  [=copy  ?=help  \
         {DIM}(prefix prefix = literal){RESET}\r\n"
    );
    let _ = std::io::stderr().flush();
}

// --- copy mode / scrollback ---------------------------------------------

fn scrollback_cap() -> usize {
    std::env::var("RAT_SCROLLBACK_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(SCROLLBACK_CAP_DEFAULT)
}

fn append_scrollback(
    buf: &std::sync::Mutex<std::collections::VecDeque<u8>>,
    data: &[u8],
    cap: usize,
) {
    let mut b = buf.lock().unwrap();
    b.extend(data.iter().copied());
    while b.len() > cap {
        b.pop_front();
    }
}

// Quick CSI-aware ANSI stripper for copy-mode rendering. Keeps text,
// newlines, tabs; drops CSI sequences and single-byte ESC escapes.
// Copy mode strips everything (not just clears/moves) because rendering
// raw cursor-movement in the alt-screen would scramble the scrollback
// view. Colors are the price we pay; grep's ANSI stripper is reused
// conceptually here but kept local to avoid a tangled dependency.
fn strip_ansi_for_view(data: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b {
            i += 1;
            if i < data.len() && data[i] == b'[' {
                i += 1;
                while i < data.len() && !(0x40..=0x7e).contains(&data[i]) {
                    i += 1;
                }
                if i < data.len() {
                    i += 1;
                }
            } else if i < data.len() {
                i += 1;
            }
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Copy mode: alt-screen scrollback viewer. Triggered by `<prefix> [`.
// Holds output_task's writes in the `pending` buffer while we render;
// on exit, the alt-screen pops back to the live view and we flush
// `pending` so the live screen catches up with anything the shell
// produced during scrolling.
async fn run_copy_mode(
    scrollback: &Arc<std::sync::Mutex<std::collections::VecDeque<u8>>>,
    in_copy: &Arc<std::sync::atomic::AtomicBool>,
    pending: &Arc<std::sync::Mutex<Vec<u8>>>,
    stdin_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) {
    use std::sync::atomic::Ordering;

    in_copy.store(true, Ordering::SeqCst);

    let raw: Vec<u8> = scrollback.lock().unwrap().iter().copied().collect();
    let stripped = strip_ansi_for_view(&raw);
    let lines: Vec<&str> = stripped.lines().collect();
    let total = lines.len();

    // Alt-screen in, hide cursor. Helper locks + releases stdout per call
    // so no StdoutLock crosses an .await (the async state machine needs
    // all held guards to be Send).
    write_stdout_sync(b"\x1b[?1049h\x1b[?25l");

    let (cols, term_rows) = terminal::size().unwrap_or((80, 24));
    let view_rows: usize = (term_rows as usize).saturating_sub(1).max(1);
    let max_top = total.saturating_sub(view_rows);
    let mut top: usize = max_top;

    render_copy_view(&lines, top, view_rows, term_rows, cols, total);

    'outer: loop {
        let Some(bytes) = stdin_rx.recv().await else {
            break;
        };
        let mut moved = false;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'q' => break 'outer,
                0x1b => {
                    // ESC [ X arrow / page keys (same-buffer case) or
                    // bare ESC (exit). Split across reads isn't handled
                    // — edge case for MVP.
                    if i + 2 < bytes.len() && bytes[i + 1] == b'[' {
                        match bytes[i + 2] {
                            b'A' => {
                                top = top.saturating_sub(1);
                                moved = true;
                            }
                            b'B' => {
                                top = (top + 1).min(max_top);
                                moved = true;
                            }
                            b'5' => {
                                top = top.saturating_sub(view_rows);
                                moved = true;
                            }
                            b'6' => {
                                top = (top + view_rows).min(max_top);
                                moved = true;
                            }
                            _ => {}
                        }
                        i += 3;
                        continue;
                    } else {
                        break 'outer;
                    }
                }
                b'j' => {
                    top = (top + 1).min(max_top);
                    moved = true;
                }
                b'k' => {
                    top = top.saturating_sub(1);
                    moved = true;
                }
                b' ' | 0x06 /* Ctrl-F */ => {
                    top = (top + view_rows).min(max_top);
                    moved = true;
                }
                b'b' | 0x02 /* Ctrl-B */ => {
                    top = top.saturating_sub(view_rows);
                    moved = true;
                }
                b'g' => {
                    top = 0;
                    moved = true;
                }
                b'G' => {
                    top = max_top;
                    moved = true;
                }
                _ => {}
            }
            i += 1;
        }
        if moved {
            render_copy_view(&lines, top, view_rows, term_rows, cols, total);
        }
    }

    // Alt-screen out, cursor back, flush pending live output.
    let pending_bytes: Vec<u8> = std::mem::take(&mut *pending.lock().unwrap());
    let mut restore = Vec::with_capacity(pending_bytes.len() + 16);
    restore.extend_from_slice(b"\x1b[?25h\x1b[?1049l");
    restore.extend_from_slice(&pending_bytes);
    write_stdout_sync(&restore);

    in_copy.store(false, Ordering::SeqCst);
}

fn write_stdout_sync(bytes: &[u8]) {
    use std::io::Write as _;
    let mut so = std::io::stdout().lock();
    let _ = so.write_all(bytes);
    let _ = so.flush();
}

fn render_copy_view(
    lines: &[&str],
    top: usize,
    view_rows: usize,
    term_rows: u16,
    cols: u16,
    total: usize,
) {
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    out.extend_from_slice(b"\x1b[H\x1b[2J");
    let end = (top + view_rows).min(total);
    for line in &lines[top..end] {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    let status = format!(
        " rat copy-mode  {end:>5}/{total:<5}  [j/k=line  space/b=page  g/G=top/bot  q=quit] "
    );
    let pad = (cols as usize).saturating_sub(visible_len(&status));
    let goto = format!("\x1b[{term_rows};1H");
    out.extend_from_slice(goto.as_bytes());
    out.extend_from_slice(b"\x1b[7m");
    out.extend_from_slice(status.as_bytes());
    out.extend_from_slice(&vec![b' '; pad]);
    out.extend_from_slice(b"\x1b[0m");
    write_stdout_sync(&out);
}

// --- detach chord --------------------------------------------------------

// The prefix can be either a single byte (Ctrl-<key>, including Ctrl-Space /
// Ctrl-@ = 0x00) or an ESC+byte pair (Alt/Meta-<key>). Ctrl-form is cheap:
// just a byte-equality scan. Meta-form needs a small state machine that
// distinguishes `ESC <key>` (chord trigger) from `ESC [ ...` (a CSI arrow
// key etc.) and from a lone ESC (which vim/readline need delivered).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefix {
    Byte(u8),
    Meta(u8),
}

fn resolve_prefix() -> Result<Prefix> {
    match std::env::var("RAT_PREFIX") {
        Ok(s) => parse_prefix(&s)
            .with_context(|| format!("parse RAT_PREFIX='{s}' (expected 'C-<key>' or 'M-<key>')")),
        Err(_) => Ok(DEFAULT_PREFIX),
    }
}

fn parse_prefix(s: &str) -> Result<Prefix> {
    let s = s.trim();
    for p in ["C-", "c-", "Ctrl-", "ctrl-"] {
        if let Some(rest) = s.strip_prefix(p) {
            return parse_ctrl_key(rest).map(Prefix::Byte);
        }
    }
    for p in ["M-", "m-", "Alt-", "alt-", "Meta-", "meta-"] {
        if let Some(rest) = s.strip_prefix(p) {
            return parse_meta_key(rest).map(Prefix::Meta);
        }
    }
    bail!("expected form 'C-<key>' or 'M-<key>', got '{s}'")
}

fn parse_ctrl_key(rest: &str) -> Result<u8> {
    // Case-insensitive named keys first.
    match rest.to_ascii_lowercase().as_str() {
        "space" | "spc" | " " | "@" => return Ok(0x00),
        _ => {}
    }
    let mut chars = rest.chars();
    let c = chars
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty key after 'C-'"))?;
    if chars.next().is_some() {
        bail!("expected single key after 'C-', got '{rest}'");
    }
    match c {
        'a'..='z' => Ok((c as u8) - b'a' + 1),
        'A'..='Z' => Ok((c as u8) - b'A' + 1),
        '\\' => Ok(0x1c),
        ']' => Ok(0x1d),
        '^' => Ok(0x1e),
        '_' => Ok(0x1f),
        _ => bail!("unsupported Ctrl- key '{c}'"),
    }
}

fn parse_meta_key(rest: &str) -> Result<u8> {
    let mut chars = rest.chars();
    let c = chars
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty key after 'M-'"))?;
    if chars.next().is_some() {
        bail!("expected single key after 'M-', got '{rest}'");
    }
    // Only alphanumeric Meta keys — punctuation like `M-[` would collide
    // with the CSI introducer and break the filter's ESC-disambiguation.
    if !c.is_ascii_alphanumeric() {
        bail!("unsupported Meta- key '{c}' (alphanumeric only)");
    }
    Ok(c as u8)
}

fn prefix_display(prefix: Prefix) -> String {
    match prefix {
        Prefix::Byte(0x00) => "Ctrl-Space".into(),
        Prefix::Byte(b) if (1..=26).contains(&b) => format!("Ctrl-{}", (b'A' + b - 1) as char),
        Prefix::Byte(0x1c) => "Ctrl-\\".into(),
        Prefix::Byte(0x1d) => "Ctrl-]".into(),
        Prefix::Byte(0x1e) => "Ctrl-^".into(),
        Prefix::Byte(0x1f) => "Ctrl-_".into(),
        Prefix::Byte(b) => format!("0x{b:02x}"),
        Prefix::Meta(c) => format!("Alt-{}", c as char),
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum ChordAction {
    #[default]
    None,
    Detach,
    DetachThenKill,
    DetachThenNew,
    DetachThenSwitch,
}

impl ChordAction {
    fn detaches(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Default, Debug)]
struct FilterOutput {
    forward: Vec<u8>,
    // The chord invoked the in-band help cheatsheet. The session continues;
    // the client just prints the line to stderr.
    help: bool,
    // The chord asked to enter copy mode. Non-terminal: the input task
    // enters the scrollback viewer inline and returns to normal forwarding
    // when it exits.
    copy_mode: bool,
    action: ChordAction,
}

enum FilterState {
    Normal,
    // Only used when prefix is Meta(_): we've seen an ESC and are waiting
    // to see if the next byte matches our Meta key. If it's not a match
    // the buffered ESC gets flushed (either on the next byte or after
    // META_FLUSH_MS so bare ESC doesn't starve inner apps).
    SawEsc,
    InChord,
}

// InputFilter is a small state machine over the stdin byte stream.
//
// With Prefix::Byte(p):
//   <p> d   → Detach       <p> D   → Detach+Kill
//   <p> c   → Detach+New   <p> s   → Detach+Switch
//   <p> ?   → Help (stay attached, print cheatsheet)
//   <p> <p> → forward a single literal prefix byte
//   <p> <x> → swallow (matches tmux / screen behaviour)
//
// With Prefix::Meta(c):
//   ESC c d / D / c / s / ?   → same chord commands
//   ESC <other>                → forward ESC + <other> verbatim
//   lone ESC (timeout)         → forward ESC
//
// State persists across buffers — the prefix and its command can straddle
// two reads from stdin without issue.
struct InputFilter {
    prefix: Prefix,
    state: FilterState,
}

impl InputFilter {
    fn new(prefix: Prefix) -> Self {
        Self {
            prefix,
            state: FilterState::Normal,
        }
    }

    fn awaiting_esc(&self) -> bool {
        matches!(self.state, FilterState::SawEsc)
    }

    fn process(&mut self, data: &[u8]) -> FilterOutput {
        let mut out = FilterOutput::default();
        out.forward.reserve(data.len());
        for &b in data {
            if self.feed(b, &mut out) {
                return out;
            }
        }
        out
    }

    // Flush any buffered state (e.g. a lone ESC in SawEsc) back to the
    // caller so it can be forwarded to the PTY. Called by the input task
    // when META_FLUSH_MS elapses without a follow-up byte.
    fn flush(&mut self) -> Vec<u8> {
        match self.state {
            FilterState::SawEsc => {
                self.state = FilterState::Normal;
                vec![0x1b]
            }
            _ => Vec::new(),
        }
    }

    // Consume one byte. Returns true if the caller should stop feeding
    // bytes from this buffer (because a terminal chord action — detach or
    // one of its variants — is ready to fire).
    fn feed(&mut self, b: u8, out: &mut FilterOutput) -> bool {
        match self.state {
            FilterState::Normal => match self.prefix {
                Prefix::Byte(p) if b == p => {
                    self.state = FilterState::InChord;
                }
                Prefix::Meta(_) if b == 0x1b => {
                    self.state = FilterState::SawEsc;
                }
                _ => out.forward.push(b),
            },
            FilterState::SawEsc => {
                // SawEsc only happens with Meta prefix, but be defensive.
                match self.prefix {
                    Prefix::Meta(p) if b == p => {
                        self.state = FilterState::InChord;
                    }
                    _ => {
                        // Not our chord trigger — flush the buffered ESC
                        // plus this byte. Arrow keys (`ESC [ A` etc.)
                        // land here too: we emit the ESC and `[`, then
                        // subsequent bytes pass through Normal.
                        out.forward.push(0x1b);
                        out.forward.push(b);
                        self.state = FilterState::Normal;
                    }
                }
            }
            FilterState::InChord => {
                self.state = FilterState::Normal;
                match b {
                    CMD_DETACH => {
                        out.action = ChordAction::Detach;
                        return true;
                    }
                    CMD_KILL => {
                        out.action = ChordAction::DetachThenKill;
                        return true;
                    }
                    CMD_NEW => {
                        out.action = ChordAction::DetachThenNew;
                        return true;
                    }
                    CMD_SWITCH => {
                        out.action = ChordAction::DetachThenSwitch;
                        return true;
                    }
                    CMD_HELP => {
                        out.help = true;
                        // Help is non-terminal: keep consuming the rest
                        // of the buffer so the user can chain commands
                        // (e.g. `? d` to peek at help then detach).
                    }
                    CMD_COPY => {
                        out.copy_mode = true;
                        // Also non-terminal — the input task drops into
                        // the scrollback viewer inline and resumes after.
                    }
                    _ => {
                        // Literal prefix echo only makes sense for byte
                        // prefixes — with Meta, the user would have to
                        // type `ESC <key> ESC <key>`, which is weird.
                        if let Prefix::Byte(p) = self.prefix
                            && b == p
                        {
                            out.forward.push(p);
                        }
                        // anything else is swallowed
                    }
                }
            }
        }
        false
    }
}

// --- output filter (keyboard-protocol enable stripping) ----------------
//
// Some TUIs running inside a rat session (Claude Code, editors using fixterms,
// kitty's keyboard protocol, xterm's modifyOtherKeys) enable richer key
// encodings via escape sequences written to their stdout. Those sequences
// reach the rat client's local terminal and switch it out of legacy encoding.
// Once that's happened, Ctrl-anything — including our detach prefix — is
// re-emitted as a multi-byte CSI sequence and our input-side byte scan never
// matches it.
//
// Fix: intercept the daemon→client byte stream and drop the mode-enabling
// sequences before they reach the local terminal. The app inside the session
// *thinks* it enabled the protocol but the terminal never flipped, so every
// keypress stays in legacy encoding and the detach chord keeps working.
//
// Stripped:
//   CSI > … u   (kitty keyboard protocol, push)
//   CSI = … u   (kitty keyboard protocol, set)
//   CSI < … u   (kitty keyboard protocol, pop)
//   CSI ? … u   (kitty keyboard protocol, query)
//   CSI > 4 … m (xterm modifyOtherKeys)
//
// Escape hatch: RAT_PASSTHROUGH_KBD=1 turns the filter into a no-op for
// users who want the richer input and are willing to exit the inner TUI
// before detaching.

fn kbd_passthrough_enabled() -> bool {
    matches!(
        std::env::var("RAT_PASSTHROUGH_KBD").as_deref(),
        Ok("1" | "true" | "yes" | "on")
    )
}

enum OutputState {
    Normal,
    Esc,
    Csi,
}

struct OutputFilter {
    state: OutputState,
    // Holds the in-progress escape sequence (starting with ESC) so we can
    // either drop it (if it's a keyboard-protocol sequence) or re-emit it
    // intact (if it's anything else, or malformed).
    buf: Vec<u8>,
    passthrough: bool,
}

impl OutputFilter {
    fn new(passthrough: bool) -> Self {
        Self {
            state: OutputState::Normal,
            buf: Vec::new(),
            passthrough,
        }
    }

    fn process(&mut self, data: &[u8]) -> Vec<u8> {
        if self.passthrough {
            return data.to_vec();
        }
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            match self.state {
                OutputState::Normal => {
                    if b == 0x1b {
                        self.buf.clear();
                        self.buf.push(b);
                        self.state = OutputState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                OutputState::Esc => {
                    self.buf.push(b);
                    if b == b'[' {
                        self.state = OutputState::Csi;
                    } else {
                        // Not a CSI — flush buffered ESC + this byte.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = OutputState::Normal;
                    }
                }
                OutputState::Csi => {
                    self.buf.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        // Final byte; decide whether to strip.
                        if !should_strip_csi(&self.buf) {
                            out.extend_from_slice(&self.buf);
                        }
                        self.buf.clear();
                        self.state = OutputState::Normal;
                    } else if !(0x20..=0x3f).contains(&b) {
                        // Malformed CSI (e.g., ESC or control byte mid-sequence).
                        // Flush what we have and reset.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = OutputState::Normal;
                    }
                    // else: still accumulating params / intermediates.
                }
            }
        }
        out
    }
}

// `buf` is a complete CSI starting with ESC '[' and ending with a final
// byte in 0x40..=0x7e. Decide whether it's a keyboard-protocol sequence
// we should drop.
fn should_strip_csi(buf: &[u8]) -> bool {
    if buf.len() < 3 {
        return false;
    }
    let body = &buf[2..];
    let final_byte = *body.last().unwrap();
    let params = &body[..body.len() - 1];

    match final_byte {
        b'u' => {
            // Kitty keyboard protocol: CSI > / = / < / ? … u
            matches!(params.first(), Some(b'>' | b'=' | b'<' | b'?'))
        }
        b'm' => {
            // xterm modifyOtherKeys: CSI > 4 [; Pm] m
            if params.first() != Some(&b'>') {
                return false;
            }
            let rest = &params[1..];
            let first_param = rest.split(|&c| c == b';').next().unwrap_or(b"");
            first_param == b"4"
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prefix_letters() {
        assert_eq!(parse_prefix("C-a").unwrap(), Prefix::Byte(0x01));
        assert_eq!(parse_prefix("C-b").unwrap(), Prefix::Byte(0x02));
        assert_eq!(parse_prefix("ctrl-z").unwrap(), Prefix::Byte(0x1a));
        assert_eq!(parse_prefix("Ctrl-A").unwrap(), Prefix::Byte(0x01));
    }

    #[test]
    fn parse_prefix_punctuation() {
        assert_eq!(parse_prefix("C-\\").unwrap(), Prefix::Byte(0x1c));
        assert_eq!(parse_prefix("C-]").unwrap(), Prefix::Byte(0x1d));
    }

    #[test]
    fn parse_prefix_ctrl_space_aliases() {
        // All three spellings collapse to 0x00 (NUL / Ctrl-@).
        assert_eq!(parse_prefix("C-Space").unwrap(), Prefix::Byte(0x00));
        assert_eq!(parse_prefix("c-space").unwrap(), Prefix::Byte(0x00));
        assert_eq!(parse_prefix("C-@").unwrap(), Prefix::Byte(0x00));
    }

    #[test]
    fn parse_prefix_meta() {
        assert_eq!(parse_prefix("M-a").unwrap(), Prefix::Meta(b'a'));
        assert_eq!(parse_prefix("Alt-q").unwrap(), Prefix::Meta(b'q'));
        assert_eq!(parse_prefix("meta-Z").unwrap(), Prefix::Meta(b'Z'));
    }

    #[test]
    fn parse_prefix_rejects_garbage() {
        assert!(parse_prefix("").is_err());
        assert!(parse_prefix("a").is_err());
        assert!(parse_prefix("C-").is_err());
        assert!(parse_prefix("C-ab").is_err());
        assert!(parse_prefix("C-1").is_err());
        // Meta-[ would collide with CSI introducer — reject.
        assert!(parse_prefix("M-[").is_err());
        assert!(parse_prefix("M-").is_err());
    }

    // --- InputFilter: Byte-prefix tests ---

    #[test]
    fn filter_passes_normal_input() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        let out = f.process(b"hello");
        assert_eq!(out.forward, b"hello");
        assert_eq!(out.action, ChordAction::None);
    }

    #[test]
    fn filter_detects_chord_detach() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        let out = f.process(&[b'a', 0x01, b'd']);
        assert_eq!(out.forward, b"a");
        assert_eq!(out.action, ChordAction::Detach);
    }

    #[test]
    fn filter_chord_new_switch_kill() {
        for (key, want) in [
            (b'c', ChordAction::DetachThenNew),
            (b's', ChordAction::DetachThenSwitch),
            (b'D', ChordAction::DetachThenKill),
        ] {
            let mut f = InputFilter::new(Prefix::Byte(0x01));
            let out = f.process(&[0x01, key]);
            assert_eq!(out.action, want, "chord {}", key as char);
        }
    }

    #[test]
    fn filter_help_is_non_terminal() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        // `<p> ? a b c` → help fires, then `abc` forwards normally.
        let out = f.process(&[0x01, b'?', b'a', b'b', b'c']);
        assert!(out.help);
        assert_eq!(out.action, ChordAction::None);
        assert_eq!(out.forward, b"abc");
    }

    #[test]
    fn filter_help_then_detach_in_same_buffer() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        let out = f.process(&[0x01, b'?', 0x01, b'd']);
        assert!(out.help);
        assert_eq!(out.action, ChordAction::Detach);
    }

    #[test]
    fn filter_passes_literal_prefix_on_double() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        let out = f.process(&[0x01, 0x01, b'x']);
        assert_eq!(out.forward, &[0x01, b'x']);
        assert_eq!(out.action, ChordAction::None);
    }

    #[test]
    fn filter_swallows_unknown_chord() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        let out = f.process(&[b'a', 0x01, b'z', b'b']);
        assert_eq!(out.forward, b"ab");
        assert_eq!(out.action, ChordAction::None);
    }

    #[test]
    fn filter_state_spans_buffers() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        let out1 = f.process(&[b'x', 0x01]);
        assert_eq!(out1.forward, b"x");
        assert_eq!(out1.action, ChordAction::None);
        let out2 = f.process(b"d");
        assert!(out2.forward.is_empty());
        assert_eq!(out2.action, ChordAction::Detach);
    }

    #[test]
    fn filter_stops_consuming_after_detach() {
        let mut f = InputFilter::new(Prefix::Byte(0x01));
        // Any bytes after the detach command in the same buffer are
        // dropped — we're disconnecting anyway.
        let out = f.process(&[0x01, b'd', b'z', b'z']);
        assert!(out.forward.is_empty());
        assert_eq!(out.action, ChordAction::Detach);
    }

    // --- InputFilter: Meta-prefix tests ---

    #[test]
    fn meta_filter_passes_normal_input() {
        let mut f = InputFilter::new(Prefix::Meta(b'a'));
        let out = f.process(b"hello");
        assert_eq!(out.forward, b"hello");
        assert_eq!(out.action, ChordAction::None);
    }

    #[test]
    fn meta_filter_detects_chord_detach() {
        let mut f = InputFilter::new(Prefix::Meta(b'a'));
        let out = f.process(&[0x1b, b'a', b'd']);
        assert!(out.forward.is_empty());
        assert_eq!(out.action, ChordAction::Detach);
    }

    #[test]
    fn meta_filter_forwards_non_chord_esc_pair() {
        let mut f = InputFilter::new(Prefix::Meta(b'a'));
        // ESC + non-matching byte flushes as-is (e.g. user typed Alt-x).
        let out = f.process(&[0x1b, b'x', b'y']);
        assert_eq!(out.forward, &[0x1b, b'x', b'y']);
        assert_eq!(out.action, ChordAction::None);
    }

    #[test]
    fn meta_filter_forwards_arrow_key() {
        // CSI (ESC [ A) — ESC then `[` is not our prefix, so flush ESC +
        // `[`, then `A` passes through Normal.
        let mut f = InputFilter::new(Prefix::Meta(b'a'));
        let out = f.process(&[0x1b, b'[', b'A']);
        assert_eq!(out.forward, &[0x1b, b'[', b'A']);
        assert_eq!(out.action, ChordAction::None);
    }

    #[test]
    fn meta_filter_flush_bare_esc() {
        let mut f = InputFilter::new(Prefix::Meta(b'a'));
        let out = f.process(&[0x1b]);
        assert!(out.forward.is_empty());
        assert!(f.awaiting_esc());
        let flushed = f.flush();
        assert_eq!(flushed, vec![0x1b]);
        assert!(!f.awaiting_esc());
    }

    #[test]
    fn meta_filter_swallows_unknown_chord() {
        // `ESC a x` — `ESC a` enters chord, `x` is an unknown chord
        // command and gets swallowed. No literal-prefix echo on Meta.
        let mut f = InputFilter::new(Prefix::Meta(b'a'));
        let out = f.process(&[0x1b, b'a', b'x']);
        assert!(out.forward.is_empty());
        assert_eq!(out.action, ChordAction::None);
    }

    // --- prefix_display ---

    #[test]
    fn prefix_display_covers_new_forms() {
        assert_eq!(prefix_display(Prefix::Byte(0x00)), "Ctrl-Space");
        assert_eq!(prefix_display(Prefix::Byte(0x01)), "Ctrl-A");
        assert_eq!(prefix_display(Prefix::Meta(b'a')), "Alt-a");
    }

    // --- OutputFilter tests ---

    #[test]
    fn output_passes_plain_bytes() {
        let mut f = OutputFilter::new(false);
        assert_eq!(f.process(b"hello world"), b"hello world");
    }

    #[test]
    fn output_strips_kitty_push() {
        // CSI > 1 u (enable kitty keyboard protocol, flags=1)
        let mut f = OutputFilter::new(false);
        let out = f.process(b"before\x1b[>1uafter");
        assert_eq!(out, b"beforeafter");
    }

    #[test]
    fn output_strips_kitty_pop() {
        let mut f = OutputFilter::new(false);
        assert_eq!(f.process(b"x\x1b[<uy"), b"xy");
        // CSI < N u pop-N variant
        let mut f2 = OutputFilter::new(false);
        assert_eq!(f2.process(b"x\x1b[<1uy"), b"xy");
    }

    #[test]
    fn output_strips_kitty_set_and_query() {
        let mut f = OutputFilter::new(false);
        assert_eq!(f.process(b"\x1b[=15;1u"), b"");
        assert_eq!(f.process(b"\x1b[?u"), b"");
    }

    #[test]
    fn output_strips_modify_other_keys() {
        let mut f = OutputFilter::new(false);
        // CSI > 4 ; 2 m
        assert_eq!(f.process(b"\x1b[>4;2m"), b"");
        // CSI > 4 ; 0 m (disable)
        assert_eq!(f.process(b"\x1b[>4;0m"), b"");
        // CSI > 4 m (reset, no second param)
        assert_eq!(f.process(b"\x1b[>4m"), b"");
    }

    #[test]
    fn output_preserves_unrelated_csi() {
        let mut f = OutputFilter::new(false);
        // SGR color — final byte 'm' but no '>' private intermediate.
        assert_eq!(f.process(b"\x1b[31mred\x1b[0m"), b"\x1b[31mred\x1b[0m");
        // Cursor position — final byte 'H'.
        assert_eq!(f.process(b"\x1b[5;10H"), b"\x1b[5;10H");
        // Other xterm private mode on 'm' with different first param.
        assert_eq!(f.process(b"\x1b[>2;1m"), b"\x1b[>2;1m");
    }

    #[test]
    fn output_preserves_bare_escape_and_non_csi() {
        let mut f = OutputFilter::new(false);
        // ESC 7 (save cursor) — not a CSI.
        assert_eq!(f.process(b"\x1b7"), b"\x1b7");
        // OSC — not CSI (starts with ESC ]).
        assert_eq!(f.process(b"\x1b]0;title\x07"), b"\x1b]0;title\x07");
    }

    #[test]
    fn output_handles_split_across_buffers() {
        let mut f = OutputFilter::new(false);
        // Feed `\x1b[>1u` one byte at a time — nothing should emit until
        // we know the sequence is kitty's keyboard push, at which point
        // it's dropped entirely.
        assert_eq!(f.process(b"\x1b"), b"");
        assert_eq!(f.process(b"["), b"");
        assert_eq!(f.process(b">"), b"");
        assert_eq!(f.process(b"1"), b"");
        assert_eq!(f.process(b"u"), b"");
        assert_eq!(f.process(b"next"), b"next");
    }

    #[test]
    fn output_flushes_non_matching_split() {
        let mut f = OutputFilter::new(false);
        // Color sequence split mid-stream — must emit in full.
        assert_eq!(f.process(b"\x1b[3"), b"");
        assert_eq!(f.process(b"1m"), b"\x1b[31m");
    }

    #[test]
    fn output_passthrough_is_identity() {
        let mut f = OutputFilter::new(true);
        // Even the stripped forms should pass through unchanged.
        assert_eq!(
            f.process(b"\x1b[>1u\x1b[<u\x1b[>4;2m"),
            b"\x1b[>1u\x1b[<u\x1b[>4;2m"
        );
    }

    #[test]
    fn output_recovers_from_malformed_csi() {
        let mut f = OutputFilter::new(false);
        // ESC inside a CSI params area — abort the current sequence, emit,
        // then handle the new ESC fresh.
        let got = f.process(b"\x1b[3\x1b[31m");
        // First partial `\x1b[3` should be flushed when the inner ESC arrives;
        // second `\x1b[31m` is an SGR and passes through.
        assert!(got.ends_with(b"\x1b[31m"));
    }
}
