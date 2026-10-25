//! Private, user-owned Unix socket. Length-prefixed JSON with bounded frames,
//! read/write deadlines and no execution of client-supplied shell commands.
use crate::{Command, Error, Result, Snapshot};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub error: Option<String>,
    pub data: serde_json::Value,
}

impl Reply {
    pub fn error(error: impl ToString) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            data: serde_json::Value::Null,
        }
    }
    pub fn snapshot(command: &Command, snapshot: &Snapshot) -> Self {
        let data = match command {
            Command::ListWindows => serde_json::to_value(&snapshot.windows),
            Command::ListWorkspaces => serde_json::to_value(&snapshot.workspaces),
            Command::ListMonitors => serde_json::to_value(&snapshot.monitors),
            Command::ListApps => {
                let apps: std::collections::BTreeMap<_, _> = snapshot
                    .windows
                    .iter()
                    .map(|w| (w.pid, (&w.app_name, &w.app_id)))
                    .collect();
                serde_json::to_value(apps)
            }
            _ => serde_json::to_value(snapshot),
        };
        match data {
            Ok(data) => Self {
                ok: true,
                error: None,
                data,
            },
            Err(e) => Self::error(e),
        }
    }
}

pub fn socket_path(support: &Path) -> PathBuf {
    support.join("window-manager/control.sock")
}

pub fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| Error(e.to_string()))?;
    if bytes.len() > MAX_FRAME {
        return Err(Error("window manager message exceeds 1 MiB".into()));
    }
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .map_err(|e| Error(e.to_string()))
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> Result<T> {
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .map_err(|e| Error(e.to_string()))?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(Error("invalid window manager message length".into()));
    }
    let mut bytes = vec![0; len];
    stream
        .read_exact(&mut bytes)
        .map_err(|e| Error(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| Error(e.to_string()))
}

pub fn request(support: &Path, args: Vec<String>) -> Result<Reply> {
    let mut stream = UnixStream::connect(socket_path(support)).map_err(|e| {
        Error(format!(
            "Sift window manager is not running; enable it in Sift Settings ({e})"
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| Error(e.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|e| Error(e.to_string()))?;
    write_frame(
        &mut stream,
        &Request {
            version: VERSION,
            args,
        },
    )?;
    read_frame(&mut stream)
}
