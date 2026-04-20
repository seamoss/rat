use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn short(&self) -> String {
        self.0.to_string()[..8].to_string()
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: Option<String>,
}

// Written to <run_dir>/<id>.meta.json on daemon start, removed on clean exit.
// Lets `rat list` discover sessions without scanning processes.
//
// The daemon writes this file exactly once, at startup. The client rewrites
// it in-place for rename / alias operations; the daemon never re-reads it,
// so no coordination is needed beyond the implicit "one writer at a time"
// that comes from the user driving these commands by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub pid: u32,
    pub sock_path: PathBuf,
    pub log_path: PathBuf,
    pub started: SystemTime,
    pub command: String,
    pub name: Option<String>,
    // Secondary labels that resolve to this session (like symlinks, but for
    // names). Serde default keeps old meta files loadable.
    #[serde(default)]
    pub aliases: Vec<String>,
}
