use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rat::event::Event;
use rat::paths;
use rat::protocol::{self, ClientMsg, DaemonMsg};
use rat::{EventLog, FileEventLog, SessionId, SessionMeta};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "rat-daemon", about = "Rat background session host")]
struct Args {
    #[arg(long)]
    id: Option<SessionId>,
    #[arg(long, default_value_t = 80)]
    cols: u16,
    #[arg(long, default_value_t = 24)]
    rows: u16,
    #[arg(long)]
    name: Option<String>,
    /// Command to run; defaults to $SHELL.
    #[arg(trailing_var_arg = true)]
    cmd: Vec<String>,
}

struct ClientState {
    session_id: SessionId,
    session_name: Option<String>,
    log: Arc<StdMutex<FileEventLog>>,
    bcast_rx: broadcast::Receiver<rat::LoggedEvent>,
    pty_in: mpsc::UnboundedSender<Vec<u8>>,
    pty_master: Arc<StdMutex<Box<dyn MasterPty + Send>>>,
    size: Arc<StdMutex<(u16, u16)>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let state_dir = paths::state_dir();
    let run_dir = paths::run_dir();
    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&run_dir)?;

    init_tracing(&paths::daemon_log_path());

    let session_id = args.id.unwrap_or_else(SessionId::new);
    let log_path = paths::log_path(&session_id.to_string());
    let sock_path = paths::sock_path(&session_id.to_string());
    let meta_path = paths::meta_path(&session_id.to_string());

    tracing::info!(%session_id, pid = std::process::id(), "daemon starting");

    let log = Arc::new(StdMutex::new(FileEventLog::open(&log_path)?));

    let shell = args
        .cmd
        .first()
        .cloned()
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
    let shell_args: Vec<String> = args.cmd.iter().skip(1).cloned().collect();
    let cols = args.cols;
    let rows = args.rows;

    let pty = native_pty_system()
        .openpty(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;

    let mut cmd = CommandBuilder::new(&shell);
    for a in &shell_args {
        cmd.arg(a);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    let env: Vec<(String, String)> = std::env::vars().collect();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.env("RAT_SESSION", session_id.to_string());
    if let Some(name) = &args.name {
        cmd.env("RAT_NAME", name);
    }

    log.lock().unwrap().append(Event::SessionStarted {
        command: shell.clone(),
        args: shell_args.clone(),
        env,
        cwd: std::env::current_dir().unwrap_or_default(),
        cols,
        rows,
    })?;

    let mut child = pty.slave.spawn_command(cmd).context("spawn shell")?;
    let child_pid = child.process_id();
    drop(pty.slave);

    // PTY input pump: tokio mpsc -> dedicated blocking thread -> master writer.
    let (pty_in_tx, mut pty_in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut pty_writer = pty.master.take_writer().context("take pty writer")?;
    std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            while let Some(data) = pty_in_rx.blocking_recv() {
                if pty_writer.write_all(&data).is_err() {
                    break;
                }
                let _ = pty_writer.flush();
            }
        })?;

    // Broadcast of LoggedEvents to attached clients.
    let (bcast_tx, _) = broadcast::channel::<rat::LoggedEvent>(4096);

    // PTY output pump: master reader -> append log -> broadcast.
    let pty_reader = pty.master.try_clone_reader().context("clone pty reader")?;
    let log_r = Arc::clone(&log);
    let bcast_r = bcast_tx.clone();
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut reader = pty_reader;
            let mut buf = [0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                let append_result = log_r.lock().unwrap().append(Event::PtyOutput {
                    data: buf[..n].to_vec(),
                });
                if let Ok(logged) = append_result {
                    let _ = bcast_r.send(logged);
                }
            }
        })?;

    let pty_master = Arc::new(StdMutex::new(pty.master));
    let size = Arc::new(StdMutex::new((cols, rows)));

    // Write discoverable session metadata.
    let meta = SessionMeta {
        id: session_id,
        pid: std::process::id(),
        sock_path: sock_path.clone(),
        log_path: log_path.clone(),
        started: SystemTime::now(),
        command: shell.clone(),
        name: args.name.clone(),
        aliases: Vec::new(),
    };
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;

    let shutdown = Arc::new(Notify::new());

    // Child exit watcher: when the PTY child exits, append SessionEnded and
    // signal daemon shutdown.
    let log_end = Arc::clone(&log);
    let bcast_end = bcast_tx.clone();
    let shutdown_end = Arc::clone(&shutdown);
    tokio::task::spawn_blocking(move || {
        let status = child.wait().ok();
        let exit_code = status.and_then(|s| s.exit_code().try_into().ok());
        if let Ok(logged) = log_end
            .lock()
            .unwrap()
            .append(Event::SessionEnded { exit_code })
        {
            let _ = bcast_end.send(logged);
        }
        shutdown_end.notify_waiters();
    });

    // Socket listener.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).context("bind unix socket")?;
    tracing::info!(path = %sock_path.display(), "listening");

    // Graceful-shutdown signals: SIGTERM (sent by `rat kill`) and SIGHUP. We
    // don't exit on them directly — we forward SIGTERM to the PTY child so the
    // existing child-exit watcher drives the normal shutdown path and the log
    // records a proper SessionEnded.
    let mut sig_term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    let mut sig_hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("install SIGHUP handler")?;

    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            _ = sig_term.recv() => {
                tracing::info!("SIGTERM → killing pty child {child_pid:?}");
                if let Some(pid) = child_pid {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                }
            }
            _ = sig_hup.recv() => {
                tracing::info!("SIGHUP → killing pty child {child_pid:?}");
                if let Some(pid) = child_pid {
                    unsafe { libc::kill(pid as i32, libc::SIGHUP); }
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let state = ClientState {
                            session_id,
                            session_name: args.name.clone(),
                            log: Arc::clone(&log),
                            bcast_rx: bcast_tx.subscribe(),
                            pty_in: pty_in_tx.clone(),
                            pty_master: Arc::clone(&pty_master),
                            size: Arc::clone(&size),
                        };
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, state).await {
                                tracing::warn!(error = %e, "client handler");
                            }
                        });
                    }
                    Err(e) => tracing::error!(error = %e, "accept"),
                }
            }
        }
    }

    // Give attached clients a moment to receive SessionEnded before tearing
    // down the listener.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&meta_path);
    tracing::info!("daemon exited cleanly");
    Ok(())
}

async fn handle_client(stream: UnixStream, mut st: ClientState) -> Result<()> {
    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let w = Arc::new(tokio::sync::Mutex::new(w));

    // First message must be Attach.
    let msg: ClientMsg = protocol::read_frame(&mut r).await?;
    let from_seq = match msg {
        ClientMsg::Attach { from_seq } => from_seq,
        _ => anyhow::bail!("expected Attach as first message"),
    };

    // Snap current seq under log lock so events appended after this point
    // arrive via the broadcast we already subscribed to. Historical events
    // with seq < snap come from the log; the forwarder filters broadcast
    // events down to seq >= snap so there are no duplicates.
    let (snap_seq, historical) = {
        let log_guard = st.log.lock().unwrap();
        let snap = log_guard.latest_seq();
        let hist = log_guard.read_from(from_seq)?;
        (snap, hist)
    };

    let (cols, rows) = *st.size.lock().unwrap();
    {
        let mut w_guard = w.lock().await;
        protocol::write_frame(
            &mut *w_guard,
            &DaemonMsg::Attached {
                session_id: st.session_id.to_string(),
                name: st.session_name.clone(),
                cols,
                rows,
                latest_seq: snap_seq,
            },
        )
        .await?;
        for ev in historical.into_iter().filter(|e| e.seq < snap_seq) {
            protocol::write_frame(&mut *w_guard, &DaemonMsg::Event { event: ev }).await?;
        }
    }

    // Broadcast forwarder.
    let w_bcast = Arc::clone(&w);
    let bcast_task = tokio::spawn(async move {
        loop {
            let ev = match st.bcast_rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => break,
            };
            if ev.seq < snap_seq {
                continue;
            }
            let is_end = matches!(ev.event, Event::SessionEnded { .. });
            let mut w_guard = w_bcast.lock().await;
            if protocol::write_frame(&mut *w_guard, &DaemonMsg::Event { event: ev })
                .await
                .is_err()
            {
                break;
            }
            if is_end {
                break;
            }
        }
    });

    // Client message loop.
    loop {
        let msg: ClientMsg = match protocol::read_frame(&mut r).await {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            ClientMsg::Attach { .. } => {}
            ClientMsg::Input { data } => {
                let _ = st
                    .log
                    .lock()
                    .unwrap()
                    .append(Event::ClientInput { data: data.clone() });
                if st.pty_in.send(data).is_err() {
                    break;
                }
            }
            ClientMsg::Resize { cols, rows } => {
                *st.size.lock().unwrap() = (cols, rows);
                let _ = st.log.lock().unwrap().append(Event::Resized { cols, rows });
                let m = Arc::clone(&st.pty_master);
                let _ = tokio::task::spawn_blocking(move || {
                    let m = m.lock().unwrap();
                    let _ = m.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                })
                .await;
            }
            ClientMsg::Detach => break,
        }
    }

    bcast_task.abort();
    Ok(())
}

fn init_tracing(path: &PathBuf) {
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_env_filter(filter)
            .with_ansi(false)
            .try_init();
    }
}
