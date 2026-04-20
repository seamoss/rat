use crate::event::{Event, LoggedEvent};
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub trait EventLog {
    fn append(&mut self, event: Event) -> Result<LoggedEvent>;
    fn read_from(&self, start_seq: u64) -> Result<Vec<LoggedEvent>>;
    fn latest_seq(&self) -> u64;
}

pub struct FileEventLog {
    path: PathBuf,
    writer: File,
    next_seq: u64,
}

impl FileEventLog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let next_seq = if path.exists() {
            let f = File::open(&path).with_context(|| format!("open {path:?}"))?;
            let mut max_seq = 0u64;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.is_empty() {
                    continue;
                }
                let ev: LoggedEvent = serde_json::from_str(&line)?;
                if ev.seq >= max_seq {
                    max_seq = ev.seq + 1;
                }
            }
            max_seq
        } else {
            0
        };

        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {path:?} for append"))?;

        Ok(Self {
            path,
            writer,
            next_seq,
        })
    }
}

impl EventLog for FileEventLog {
    fn append(&mut self, event: Event) -> Result<LoggedEvent> {
        let logged = LoggedEvent {
            seq: self.next_seq,
            timestamp: SystemTime::now(),
            event,
        };
        let line = serde_json::to_string(&logged)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.next_seq += 1;
        Ok(logged)
    }

    fn read_from(&self, start_seq: u64) -> Result<Vec<LoggedEvent>> {
        let f = File::open(&self.path)?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let ev: LoggedEvent = serde_json::from_str(&line)?;
            if ev.seq >= start_seq {
                out.push(ev);
            }
        }
        Ok(out)
    }

    fn latest_seq(&self) -> u64 {
        self.next_seq
    }
}
