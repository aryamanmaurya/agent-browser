//! Wire protocol between the CLI client and the daemon.
//!
//! Framing: 4-byte big-endian length prefix + JSON payload of a `Request` or
//! `Response`. One request per connection (the CLI opens a fresh connection
//! for each command, the daemon handles it, responds, closes).

use serde::{Deserialize, Serialize};

/// A command sent from the CLI to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Daemon health check.
    Status,
    /// Shut the daemon down (closes the browser too).
    Shutdown,
    Navigate { url: String },
    Snapshot { text_only: bool },
    Click { selector: String },
    Type { selector: String, text: String },
    Press { key: String },
    Scroll {
        direction: String, // "up" | "down" | "left" | "right"
        amount: Option<i32>,
    },
    Hover { selector: String },
    Select { selector: String, value: String },
    Resize { width: u32, height: u32 },
    ViewportPreset { preset: String }, // mobile | tablet | desktop
    WaitFor {
        wait_kind: String, // selector | text | url
        target: String,
        timeout_ms: u64,
    },
    Console,
    Cookies,
    LocalStorage { key: Option<String> },
    Network,
    Back,
    Forward,
    Reload,
    Close,
}

/// The daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok { message: String },
    Status {
        url: String,
        viewport: (u32, u32),
        chrome_pid: u32,
        uptime_secs: u64,
    },
    Snapshot {
        /// Base64-encoded PNG screenshot. Absent when `text_only` was requested
        /// or when the page has no visible content.
        #[serde(skip_serializing_if = "Option::is_none")]
        screenshot_b64: Option<String>,
        /// Visible text content of the page (document.body.innerText).
        text: String,
        /// Console entries captured since the previous snapshot/command.
        console: Vec<ConsoleEntry>,
        /// Network requests that FAILED since the last snapshot (signal, not noise).
        /// Full network log (including successes) is available via the `network` command.
        network_failures: Vec<NetworkEntry>,
        viewport: (u32, u32),
        url: String,
        title: String,
    },
    Console { entries: Vec<ConsoleEntry> },
    Cookies { cookies: Vec<Cookie> },
    LocalStorage { entries: Vec<(String, String)> },
    Network { entries: Vec<NetworkEntry> },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleEntry {
    /// "log" | "error" | "warning" | "info" | "debug"
    pub level: String,
    pub text: String,
    /// Where the message came from (JS line, or "network" for failed requests).
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEntry {
    pub request_id: String,
    pub method: String,
    pub url: String,
    /// Document, Stylesheet, Image, Script, XHR, Fetch, etc.
    pub resource_type: String,
    /// HTTP status code, if a response was received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Response body size in bytes, if loading finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoded_size: Option<i64>,
    /// True if the request failed (network error, blocked, canceled, etc.).
    pub failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
}

impl Response {
    pub fn err(msg: impl Into<String>) -> Self {
        Response::Error { message: msg.into() }
    }
    pub fn ok(msg: impl Into<String>) -> Self {
        Response::Ok { message: msg.into() }
    }
}

/// Length-prefixed frame helpers.
pub async fn write_frame<W>(w: &mut W, value: &impl Serialize) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_vec(value)?;
    let len = (json.len() as u32).to_be_bytes();
    use tokio::io::AsyncWriteExt;
    w.write_all(&len).await?;
    w.write_all(&json).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> anyhow::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        anyhow::bail!("frame too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}
