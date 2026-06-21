//! agent-browser: a persistent headless browser session for AI agents.
//!
//! The CLI is a thin client that talks to a long-lived daemon over a Unix
//! domain socket. On first use the daemon is auto-spawned (detached) and kept
//! alive across invocations so each command costs ~5ms instead of ~1-2s of
//! Chromium startup.

mod client;
mod commands;
mod daemon;
mod mcp;
mod protocol;
mod selector;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use protocol::Request;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "agent-browser",
    version,
    about = "Persistent headless browser session for AI agents (chromiumoxide-backed)"
)]
struct Cli {
    /// Path to a Chromium binary. Overrides $AGENT_BROWSER_CHROME.
    #[arg(long, global = true, env = "AGENT_BROWSER_CHROME")]
    chrome: Option<PathBuf>,

    /// Path to the daemon's Unix socket.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (usually auto-spawned; useful for debugging).
    Daemon {
        /// URL to navigate to on startup.
        #[arg(long)]
        initial_url: Option<String>,
        /// Idle shutdown timeout in seconds.
        #[arg(long, default_value_t = 600)]
        idle_timeout_secs: u64,
    },
    /// Show daemon status (URL, viewport, uptime).
    Status,
    /// Stop the running daemon.
    Stop,
    /// Navigate to a URL.
    Navigate { url: String },
    /// Capture screenshot + visible text + console logs.
    Snapshot {
        /// Skip the screenshot (text + console only). For text-only orchestrator models.
        #[arg(long)]
        text_only: bool,
    },
    /// Click an element. Selector syntax: css=..., text=..., role=button[name=...], xpath=..., or bare CSS.
    Click { selector: String },
    /// Type text into an element (focuses + clears + types).
    Type { selector: String, text: String },
    /// Press a keyboard key (e.g. Enter, Escape, Tab, a).
    Press { key: String },
    /// Scroll the page.
    Scroll {
        /// up | down | left | right
        #[arg(default_value = "down")]
        direction: String,
        /// Pixels to scroll (default 600).
        #[arg(long)]
        amount: Option<i32>,
    },
    /// Hover an element (dispatches mouseover/mouseenter/mousemove).
    Hover { selector: String },
    /// Select an option in a <select> element.
    Select { selector: String, value: String },
    /// Resize the viewport to an exact size.
    Resize { width: u32, height: u32 },
    /// Use a viewport preset.
    Viewport {
        /// mobile (375x667) | tablet (768x1024) | desktop (1280x800)
        #[arg(default_value = "desktop")]
        preset: String,
    },
    /// Wait until a condition is met (selector visible, text present, or URL matches).
    WaitFor {
        /// selector | text | url
        #[arg(long, default_value = "selector")]
        kind: String,
        /// The selector string, text to find, or URL substring.
        target: String,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
    },
    /// Dump console logs captured since the last snapshot/console drain.
    Console,
    /// Dump all network requests (success + failure) captured since the last drain.
    Network,
    /// Get cookies for the current page.
    Cookies,
    /// Get localStorage entries (all, or one key).
    LocalStorage {
        /// Specific key to fetch. If omitted, returns all entries.
        key: Option<String>,
    },
    /// Browser history back.
    Back,
    /// Browser history forward.
    Forward,
    /// Reload the current page.
    Reload,
    /// Close the session and stop the daemon.
    Close,
    /// Run as an MCP (Model Context Protocol) server over stdio. Exposes all
    /// browser commands as MCP tools for LLM clients (Claude Desktop, etc.).
    /// The snapshot tool returns screenshots as image content blocks that
    /// multimodal models can see.
    Mcp,
}

fn default_socket_path() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine cache/data directory for socket"))?;
    Ok(base.join("agent-browser").join("sock"))
}

fn resolve_chrome(cli_chrome: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = cli_chrome {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("AGENT_BROWSER_CHROME") {
        return Ok(PathBuf::from(p));
    }
    // Try common locations.
    let candidates = [
        "/home/z/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome",
        "/home/z/.cache/ms-playwright/chromium-1200/chrome-linux64/chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
    ];
    for c in candidates {
        if std::path::Path::new(c).exists() {
            return Ok(PathBuf::from(c));
        }
    }
    Err(anyhow!(
        "no Chromium binary found. Set --chrome or $AGENT_BROWSER_CHROME."
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let socket = match cli.socket.clone() {
        Some(s) => s,
        None => default_socket_path()?,
    };
    let chrome = resolve_chrome(cli.chrome.clone())?;

    match cli.cmd {
        Cmd::Daemon {
            initial_url,
            idle_timeout_secs,
        } => {
            daemon::run(daemon::DaemonOptions {
                chrome_executable: chrome,
                socket_path: socket,
                initial_url,
                idle_timeout_secs,
            })
            .await
        }
        cmd => {
            if matches!(cmd, Cmd::Mcp) {
                mcp::run(chrome).await?;
                return Ok(());
            }
            let req = build_request(cmd);
            let resp = client::send(req, &socket, &chrome).await?;
            client::print_response(resp);
            Ok(())
        }
    }
}

fn build_request(cmd: Cmd) -> Request {
    match cmd {
        Cmd::Status => Request::Status,
        Cmd::Stop | Cmd::Close => Request::Shutdown,
        Cmd::Navigate { url } => Request::Navigate { url },
        Cmd::Snapshot { text_only } => Request::Snapshot { text_only },
        Cmd::Click { selector } => Request::Click { selector },
        Cmd::Hover { selector } => Request::Hover { selector },
        Cmd::Type { selector, text } => Request::Type { selector, text },
        Cmd::Press { key } => Request::Press { key },
        Cmd::Scroll { direction, amount } => Request::Scroll { direction, amount },
        Cmd::Select { selector, value } => Request::Select { selector, value },
        Cmd::Resize { width, height } => Request::Resize { width, height },
        Cmd::Viewport { preset } => Request::ViewportPreset { preset },
        Cmd::WaitFor {
            kind,
            target,
            timeout_ms,
        } => Request::WaitFor {
            wait_kind: kind,
            target,
            timeout_ms,
        },
        Cmd::Console => Request::Console,
        Cmd::Network => Request::Network,
        Cmd::Cookies => Request::Cookies,
        Cmd::LocalStorage { key } => Request::LocalStorage { key },
        Cmd::Back => Request::Back,
        Cmd::Forward => Request::Forward,
        Cmd::Reload => Request::Reload,
        Cmd::Daemon { .. } => unreachable!(),
        Cmd::Mcp => unreachable!(),
    }
}
