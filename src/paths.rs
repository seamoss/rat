use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("rat");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/rat");
    }
    PathBuf::from(".rat")
}

pub fn run_dir() -> PathBuf {
    state_dir().join("run")
}

pub fn log_path(session_id: &str) -> PathBuf {
    state_dir().join(format!("{session_id}.log"))
}

pub fn sock_path(session_id: &str) -> PathBuf {
    run_dir().join(format!("{session_id}.sock"))
}

pub fn meta_path(session_id: &str) -> PathBuf {
    run_dir().join(format!("{session_id}.meta.json"))
}

pub fn daemon_log_path() -> PathBuf {
    state_dir().join("daemon.trace")
}
