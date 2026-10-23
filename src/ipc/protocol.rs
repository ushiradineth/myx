use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{PlayerCommand, Snapshot};
use crate::webapi::HttpMethod;

pub const VERSION: u16 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub protocol_version: u16,
    pub request_id: u64,
    pub config_path: Option<String>,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", content = "arguments", rename_all = "snake_case")]
pub enum Operation {
    Status,
    Watch {
        after: u64,
        client_id: String,
        spectrum: bool,
    },
    Unsubscribe {
        client_id: String,
    },
    Player(PlayerCommand),
    Catalog {
        method: HttpMethod,
        path: String,
        body: Option<Value>,
    },
    Authorize,
    Shutdown,
}

#[derive(Serialize, Deserialize)]
pub struct Response {
    pub protocol_version: u16,
    pub request_id: u64,
    pub result: Result<Payload, IpcError>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Payload {
    Snapshot(Box<Snapshot>),
    Accepted,
    Json(Value),
    Authorization { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    ConfigMismatch,
    SetupRequired,
    Busy,
    InvalidRequest,
    Unavailable,
    Unauthorized,
    Restricted,
    NotFound,
    Quota,
    RateLimited,
    Timeout,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcError {
    pub code: ErrorCode,
    pub message: String,
}

impl IpcError {
    pub fn new(code: ErrorCode, message: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for IpcError {}

pub fn send<T: Serialize>(stream: &mut impl Write, value: &T) -> io::Result<()> {
    let encoded = serde_json::to_vec(value).map_err(|_| io::Error::other("IPC encoding failed"))?;
    if encoded.is_empty() || encoded.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds limit",
        ));
    }
    stream.write_all(&(encoded.len() as u32).to_be_bytes())?;
    stream.write_all(&encoded)
}

pub fn receive<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<T> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds limit",
        ));
    }
    let mut encoded = vec![0; size];
    stream.read_exact(&mut encoded)?;
    serde_json::from_slice(&encoded)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Malformed IPC frame"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_frame_is_refused_before_body_read() {
        let prefix = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(receive::<Request>(&mut prefix.as_slice()).is_err());
        assert!(receive::<Request>(&mut b"\0\0\0\x03xxx".as_slice()).is_err());
    }
}
