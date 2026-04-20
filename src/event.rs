use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

// Events are the append-only record of everything that happens in a session.
// Replaying them from seq 0 reconstructs current state — this is the whole
// point of Rat: session state is not bound to the daemon process, it's in the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    SessionStarted {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    PtyOutput {
        data: Vec<u8>,
    },
    ClientInput {
        data: Vec<u8>,
    },
    Resized {
        cols: u16,
        rows: u16,
    },
    SessionEnded {
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggedEvent {
    pub seq: u64,
    pub timestamp: SystemTime,
    pub event: Event,
}
