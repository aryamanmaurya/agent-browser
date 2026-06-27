//! CLI client: connect to the daemon (auto-spawning it if absent), send a
//! single `Request`, print the `Response`.

use crate::protocol::{read_frame, write_frame, Request, Response};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;

/// Send a request to the daemon. Auto-spawns the daemon if the socket is not
/// accepting connections. `headful` only affects newly-spawned daemons; if a
/// daemon is already running on this socket, the flag is ignored.
pub async fn send(
    req: Request,
    socket_path: &Path,
    chrome_executable: &Path,
    headful: bool,
) -> Result<Response> {
    let stream = connect_or_spawn(socket_path, chrome_executable, headful).await?;
    let mut stream = stream;
    write_frame(&mut stream, &req).await?;
    let resp: Response = read_frame(&mut stream).await?;
    Ok(resp)
}

async fn connect_or_spawn(socket_path: &Path, chrome_executable: &Path, headful: bool) -> Result<UnixStream> {
    // Fast path: daemon already running.
    if let Ok(s) = UnixStream::connect(socket_path).await {
        return Ok(s);
    }
    // Spawn the daemon detached.
    spawn_daemon_detached(socket_path, chrome_executable, headful)?;
    // Wait for it to accept connections.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() >= deadline {
            let log = socket_path.with_extension("daemon.log");
            return Err(anyhow!(
                "daemon did not become ready in 20s. Check the daemon log at {}",
                log.display()
            ));
        }
        match UnixStream::connect(socket_path).await {
            Ok(s) => return Ok(s),
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
}

fn spawn_daemon_detached(socket_path: &Path, chrome_executable: &Path, headful: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = socket_path.with_extension("daemon.log");
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = log_file.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("--chrome")
        .arg(chrome_executable)
        .arg("--socket")
        .arg(socket_path);
    if headful {
        cmd.arg("--headful");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(stderr));

    // Detach into its own session/process group so it survives the CLI exit.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// Format a `Response` as LLM-friendly text on stdout.
pub fn print_response(resp: Response) {
    match resp {
        Response::Ok { message } => {
            println!("OK: {message}");
        }
        Response::Error { message } => {
            eprintln!("ERROR: {message}");
            std::process::exit(1);
        }
        Response::Status {
            url,
            viewport,
            chrome_pid,
            uptime_secs,
            headful,
        } => {
            println!("=== AGENT-BROWSER STATUS ===");
            println!("URL: {url}");
            println!("Viewport: {} x {}", viewport.0, viewport.1);
            println!("Mode: {}", if headful { "headful" } else { "headless" });
            println!("Daemon PID: {chrome_pid}");
            println!("Uptime: {uptime_secs}s");
        }
        Response::Snapshot {
            screenshot_b64,
            text,
            console,
            network_failures,
            viewport,
            url,
            title,
        } => {
            println!("=== AGENT-BROWSER SNAPSHOT ===");
            println!("URL: {url}");
            println!("Title: {title}");
            println!("Viewport: {} x {}", viewport.0, viewport.1);
            if console.is_empty() {
                println!("=== CONSOLE (empty) ===");
            } else {
                println!("=== CONSOLE (since last snapshot) ===");
                for e in &console {
                    println!("[{}] {} ({})", e.level, e.text, e.source);
                }
            }
            if network_failures.is_empty() {
                println!("=== NETWORK FAILURES (none) ===");
            } else {
                println!("=== NETWORK FAILURES ===");
                for n in &network_failures {
                    let err = n.error_text.as_deref().unwrap_or("unknown");
                    println!("{} {} → FAILED ({})", n.method, n.url, err);
                }
            }
            println!("=== VISIBLE TEXT ===");
            // Trim very long text to keep output manageable.
            let trimmed = trim_text(&text, 8000);
            println!("{trimmed}");
            match &screenshot_b64 {
                Some(b64) => {
                    println!(
                        "=== SCREENSHOT (base64 PNG, {} bytes) ===",
                        b64.len()
                    );
                    println!("{b64}");
                }
                None => {
                    println!("=== SCREENSHOT (skipped — text-only mode) ===");
                }
            }
            println!("=== END SNAPSHOT ===");
        }
        Response::Console { entries } => {
            if entries.is_empty() {
                println!("=== CONSOLE (empty) ===");
            } else {
                println!("=== CONSOLE ===");
                for e in &entries {
                    println!("[{}] {} ({})", e.level, e.text, e.source);
                }
            }
        }
        Response::Cookies { cookies } => {
            if cookies.is_empty() {
                println!("=== COOKIES (none) ===");
            } else {
                println!("=== COOKIES ===");
                for c in &cookies {
                    println!(
                        "{}={} (domain={}, path={}, secure={}, http_only={})",
                        c.name, c.value, c.domain, c.path, c.secure, c.http_only
                    );
                }
            }
        }
        Response::LocalStorage { entries } => {
            if entries.is_empty() {
                println!("=== LOCALSTORAGE (empty) ===");
            } else {
                println!("=== LOCALSTORAGE ===");
                for (k, v) in &entries {
                    let vdisplay = if v == "null" {
                        "<null>".to_string()
                    } else {
                        v.clone()
                    };
                    println!("{k} = {vdisplay}");
                }
            }
        }
        Response::Network { entries } => {
            if entries.is_empty() {
                println!("=== NETWORK (empty) ===");
            } else {
                println!("=== NETWORK ({} requests) ===", entries.len());
                for n in &entries {
                    if n.failed {
                        let err = n.error_text.as_deref().unwrap_or("unknown");
                        println!(
                            "{} {} → FAILED ({}) [{}]",
                            n.method, n.url, err, n.resource_type
                        );
                    } else {
                        let status = n.status.unwrap_or(0);
                        let size = n
                            .encoded_size
                            .map(|s| format!("{s}B"))
                            .unwrap_or_else(|| "?".to_string());
                        let mime = n.mime_type.as_deref().unwrap_or("");
                        println!(
                            "{} {} → {} ({} {}) [{}]",
                            n.method, n.url, status, mime, size, n.resource_type
                        );
                        if let Some(body) = &n.body {
                            let preview = if body.len() > 200 { format!("{}...", &body[..200]) } else { body.clone() };
                            println!("    body: {preview}");
                        }
                    }
                }
            }
        }
        Response::Pdf { path, size_bytes } => {
            println!("=== PDF SAVED ===");
            println!("Path: {path}");
            println!("Size: {size_bytes} bytes");
        }
        Response::Inspect { info } => {
            println!("=== ELEMENT INSPECTION ===");
            println!("Tag: <{}>", info.tag);
            if !info.attributes.is_empty() {
                println!("Attributes:");
                for (k, v) in &info.attributes {
                    println!("  {k}=\"{v}\"");
                }
            }
            if !info.text.is_empty() {
                println!("Text: {}", info.text);
            }
            let bb = info.bounding_box;
            println!("Bounding box: x={:.0} y={:.0} w={:.0} h={:.0}", bb.0, bb.1, bb.2, bb.3);
            if let Some(role) = &info.aria_role {
                println!("ARIA role: {role}");
            }
            if let Some(label) = &info.aria_label {
                println!("ARIA label: {label}");
            }
            if !info.computed_styles.is_empty() {
                println!("Computed styles:");
                for (k, v) in &info.computed_styles {
                    println!("  {k}: {v}");
                }
            }
        }
        Response::Accessibility { tree } => {
            println!("=== ACCESSIBILITY TREE ===");
            if tree.is_empty() {
                println!("(empty)");
            } else {
                print!("{tree}");
            }
        }
        Response::Har { path, size_bytes, request_count } => {
            println!("=== HAR EXPORTED ===");
            println!("Path: {path}");
            println!("Size: {size_bytes} bytes");
            println!("Requests: {request_count}");
        }
        Response::TabList { tabs, active_id } => {
            println!("=== TABS ({}) ===", tabs.len());
            for t in &tabs {
                let marker = if t.active { " *" } else { "  " };
                println!("{marker} {}  {}  {}", t.id, t.url, t.title);
            }
            println!("Active: {active_id}");
        }
        Response::TabId { id, url } => {
            println!("=== TAB CREATED ===");
            println!("ID: {id}");
            println!("URL: {url}");
        }
    }
}

fn trim_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s[..s.char_indices().take(max).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(max)].to_string();
    out.push_str("\n... [truncated]");
    out
}

// ==================== Session management ====================

/// Validate a session name. Must be filesystem-safe: [a-zA-Z0-9_-], max 64 chars.
pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name cannot be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("session name too long (max 64 chars): {name}");
    }
    if name == "all" {
        // "all" is reserved for `--session all stop`
        return Ok(());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        anyhow::bail!(
            "invalid session name {:?} (allowed: a-z, A-Z, 0-9, _, -, max 64 chars)",
            name
        );
    }
    Ok(())
}

/// Compute the socket path for a given session name.
pub fn session_socket_path(session: &str) -> Result<std::path::PathBuf> {
    validate_session_name(session)?;
    let base = dirs::cache_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine cache/data directory for socket"))?;
    Ok(base.join("agent-browser").join(format!("{session}.sock")))
}

/// One session's info, as returned by `list_sessions`.
pub struct SessionInfo {
    pub name: String,
    pub url: String,
    pub headful: bool,
    pub uptime_secs: u64,
}

/// List all running sessions by scanning the socket directory and probing each.
pub async fn list_sessions() -> Result<Vec<SessionInfo>> {
    let base = dirs::cache_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine cache/data directory"))?;
    let dir = base.join("agent-browser");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        // Look for *.sock files (not *.log files).
        if path.extension().and_then(|s| s.to_str()) != Some("sock") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        // Try to connect and get status.
        match UnixStream::connect(&path).await {
            Ok(mut stream) => {
                if write_frame(&mut stream, &Request::Status).await.is_ok() {
                    if let Ok(resp) = read_frame::<_, Response>(&mut stream).await {
                        if let Response::Status { url, headful, uptime_secs, .. } = resp {
                            sessions.push(SessionInfo {
                                name,
                                url,
                                headful,
                                uptime_secs,
                            });
                        }
                    }
                }
            }
            Err(_) => {
                // Socket exists but no daemon accepting — skip (or stale).
            }
        }
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

/// Print the session list in a table format.
pub fn print_sessions(sessions: &[SessionInfo]) {
    if sessions.is_empty() {
        println!("No active sessions.");
        return;
    }
    println!("{:<16} {:<10} {:<40} {:<10}", "SESSION", "MODE", "URL", "UPTIME");
    println!("{}", "-".repeat(80));
    for s in sessions {
        let mode = if s.headful { "headful" } else { "headless" };
        let url_display = if s.url.len() > 38 {
            format!("{}...", &s.url[..35])
        } else {
            s.url.clone()
        };
        let uptime = format!("{}m {}s", s.uptime_secs / 60, s.uptime_secs % 60);
        println!("{:<16} {:<10} {:<40} {:<10}", s.name, mode, url_display, uptime);
    }
    println!("\n{} session(s) active.", sessions.len());
}
