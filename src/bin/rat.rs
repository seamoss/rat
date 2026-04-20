use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
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

const DETACH_KEY: u8 = 0x1c; // Ctrl-\

#[derive(Parser)]
#[command(name = "rat", about = "Rat — cloud-terminal session multiplexer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a new session and attach to it.
    New {
        /// Optional human-friendly name (also exported as $RAT_NAME).
        #[arg(short, long)]
        name: Option<String>,
        /// Command to run in the new session; defaults to $SHELL.
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
    /// Attach to an existing session by name or id prefix.
    Attach {
        /// Session name, or UUID / unique UUID prefix.
        id: String,
    },
    /// List running sessions.
    List,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::New { name, cmd } => new(name, cmd).await,
        Cmd::Attach { id } => {
            let session_id = resolve_session(&id)?;
            attach(session_id).await
        }
        Cmd::List => {
            if std::io::stdout().is_terminal() {
                list_pick().await
            } else {
                list_text()
            }
        }
        Cmd::Replay { path } => replay(&path),
        Cmd::Init { shell } => init(&shell),
        Cmd::Kill { id, yes } => kill(&id, yes),
    }
}

fn kill(query: &str, yes: bool) -> Result<()> {
    // Locate metadata by UUID or name. We can't rely on resolve_session alone
    // because it filters out dead sessions — and one use of `kill` is cleaning
    // up stale state from a crashed daemon.
    let metas = read_all_metas()?;
    let matches: Vec<_> = metas
        .iter()
        .filter(|m| {
            m.name.as_deref() == Some(query) || m.id.to_string().starts_with(query)
        })
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

async fn new(name: Option<String>, cmd: Vec<String>) -> Result<()> {
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

    attach(session_id).await
}

async fn attach(session_id: SessionId) -> Result<()> {
    let sock = paths::sock_path(&session_id.to_string());
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connect {}", sock.display()))?;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let w = Arc::new(tokio::sync::Mutex::new(w));

    protocol::write_frame(
        &mut *w.lock().await,
        &ClientMsg::Attach { from_seq: 0 },
    )
    .await?;

    let first: DaemonMsg = protocol::read_frame(&mut r).await?;
    let (sid, sname) = match first {
        DaemonMsg::Attached {
            session_id, name, ..
        } => (session_id, name),
        DaemonMsg::Error { message } => bail!("daemon error: {message}"),
        _ => bail!("expected Attached first"),
    };

    print_attach_banner(&sid, sname.as_deref());

    // Resize to match this client's terminal (in case we reattached from a
    // different-sized terminal than the one that started the session).
    protocol::write_frame(
        &mut *w.lock().await,
        &ClientMsg::Resize { cols, rows },
    )
    .await?;

    let _raw = RawModeGuard::enable()?;

    let done = Arc::new(Notify::new());
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exit_code = Arc::new(std::sync::Mutex::new(None::<i32>));

    // output_task: daemon → stdout
    let done_o = Arc::clone(&done);
    let ended_o = Arc::clone(&ended);
    let exit_code_o = Arc::clone(&exit_code);
    let output_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        loop {
            let msg: DaemonMsg = match protocol::read_frame(&mut r).await {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                DaemonMsg::Event { event } => match event.event {
                    Event::PtyOutput { data } => {
                        if stdout.write_all(&data).await.is_err() {
                            break;
                        }
                        let _ = stdout.flush().await;
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

    // stdin reader thread → channel (raw bytes, with Ctrl-\ as detach).
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

    // input_task: channel → daemon Input, with detach-key detection.
    let w_i = Arc::clone(&w);
    let done_i = Arc::clone(&done);
    let input_task = tokio::spawn(async move {
        while let Some(data) = stdin_rx.recv().await {
            if let Some(pos) = data.iter().position(|&b| b == DETACH_KEY) {
                if pos > 0 {
                    let _ = protocol::write_frame(
                        &mut *w_i.lock().await,
                        &ClientMsg::Input {
                            data: data[..pos].to_vec(),
                        },
                    )
                    .await;
                }
                let _ = protocol::write_frame(&mut *w_i.lock().await, &ClientMsg::Detach).await;
                break;
            }
            if protocol::write_frame(&mut *w_i.lock().await, &ClientMsg::Input { data })
                .await
                .is_err()
            {
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

    if ended.load(std::sync::atomic::Ordering::SeqCst) {
        print_session_ended(&sid, sname.as_deref(), *exit_code.lock().unwrap());
    } else {
        print_detach_banner(&sid, sname.as_deref());
    }
    Ok(())
}

fn list_text() -> Result<()> {
    let mut metas = read_all_metas()?;
    if metas.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    metas.sort_by_key(|m| m.started);
    println!(
        "{:<10} {:<16} {:<8} {:<10} {:<14} {}",
        "ID", "NAME", "PID", "STATE", "STARTED", "COMMAND"
    );
    for m in metas {
        let alive = process_alive(m.pid);
        let state = if alive { "running" } else { "dead" };
        let name = m.name.clone().unwrap_or_else(|| "-".into());
        println!(
            "{:<10} {:<16} {:<8} {:<10} {:<14} {}",
            m.id.short(),
            truncate(&name, 16),
            m.pid,
            state,
            format_started(m.started),
            m.command
        );
    }
    Ok(())
}

async fn list_pick() -> Result<()> {
    let mut alive: Vec<SessionMeta> = read_all_metas()?
        .into_iter()
        .filter(|m| process_alive(m.pid))
        .collect();
    if alive.is_empty() {
        println!("no running sessions");
        return Ok(());
    }
    alive.sort_by_key(|m| m.started);

    // Separate the raw-mode/alt-screen scope from the await below: we MUST
    // restore the terminal before calling attach(), otherwise attach()'s own
    // raw-mode setup collides with our own.
    let selected = run_picker(&alive)?;

    if let Some(id) = selected {
        attach(id).await?;
    }
    Ok(())
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
                        if cursor > 0 {
                            cursor -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if cursor + 1 < items.len() {
                            cursor += 1;
                        }
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

fn draw_picker(
    stdout: &mut std::io::Stdout,
    items: &[SessionMeta],
    cursor: usize,
) -> Result<()> {
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    writeln!(
        stdout,
        "{ORANGE}rat · select session{RESET}   {DIM}↑↓ move · enter attach · q/esc cancel{RESET}\r"
    )?;
    writeln!(stdout, "\r")?;
    writeln!(
        stdout,
        "  {DIM}  {:<10} {:<16} {:<14} {}{RESET}\r",
        "ID", "NAME", "STARTED", "COMMAND"
    )?;
    for (i, m) in items.iter().enumerate() {
        let name = m.name.clone().unwrap_or_else(|| "-".into());
        let row = format!(
            "{:<10} {:<16} {:<14} {}",
            m.id.short(),
            truncate(&name, 16),
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
        if m.name.as_deref() == Some(name) && process_alive(m.pid) {
            return Ok(Some(m));
        }
    }
    Ok(None)
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
    // Otherwise match against live sessions by name (exact) or UUID prefix.
    // Dead-pid metas are ignored — you can't attach to a dead daemon anyway.
    let mut by_name = Vec::new();
    let mut by_prefix = Vec::new();
    for m in read_all_metas()? {
        if !process_alive(m.pid) {
            continue;
        }
        if m.name.as_deref() == Some(query) {
            by_name.push(m.id);
        } else if m.id.to_string().starts_with(query) {
            by_prefix.push(m.id);
        }
    }
    // Exact name match takes precedence.
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

fn print_attach_banner(session_id: &str, name: Option<&str>) {
    let short = &session_id[..session_id.len().min(8)];
    // OSC 2: set terminal title.
    eprint!("\x1b]2;[rat] {short}\x07");

    let version = env!("CARGO_PKG_VERSION");
    eprintln!();
    eprintln!("{}", frame_top());
    eprintln!("{}", frame_blank());
    eprintln!(
        "{}",
        frame_row(&format!("   {ORANGE}█▀█ ▄▀█ ▀█▀{RESET}"))
    );
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
    eprintln!(
        "{}",
        frame_row(&format!(
            "   detach:   {BOLD}Ctrl-\\{RESET}   {DIM}(exit / Ctrl-D to end session){RESET}"
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
            name.map(|n| format!(" {DIM}({n}){RESET}")).unwrap_or_default()
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
            name.map(|n| format!(" {DIM}({n}){RESET}")).unwrap_or_default()
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
