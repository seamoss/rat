use crate::event::LoggedEvent;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Maximum frame size — 16 MiB is ample for LoggedEvent payloads and guards
// against runaway allocation if the wire is corrupted.
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    // First message on a connection — asks for replay from `from_seq` onward,
    // then a live stream of future events.
    Attach { from_seq: u64 },
    Input { data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Detach,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonMsg {
    Attached {
        session_id: String,
        name: Option<String>,
        cols: u16,
        rows: u16,
        latest_seq: u64,
    },
    Event {
        event: LoggedEvent,
    },
    Error {
        message: String,
    },
    // Sent when the PTY child exits; client should clean up and disconnect.
    SessionEnded {
        exit_code: Option<i32>,
    },
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(msg)?;
    if bytes.len() > MAX_FRAME {
        bail!("frame too large: {}", bytes.len());
    }
    let len = (bytes.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).context("deserialize frame")
}
