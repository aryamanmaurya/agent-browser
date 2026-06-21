//! CLI client: connect to the daemon (auto-spawning it if absent), send a
//! single `Request`, print the `Response`.

use crate::protocol::{read_frame, write_frame, Request, Response};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;

/// Send a request to the daemon. Auto-spawns the daemon if the socket is not
/// accepting connections.
pub async fn send(
    req: Request,
    socket_path: &Path,
    chrome_executable: &Path,
) -> Result<Response> {
    let stream = connect_or_spawn(socket_path, chrome_executable).await?;
    let mut stream = stream;
    write_frame(&mut stream, &req).await?;
    let resp: Response = read_frame(&mut stream).await?;
    Ok(resp)
}

async fn connect_or_spawn(socket_path: &Path, chrome_executable: &Path) -> Result<UnixStream> {
    // Fast path: daemon already running.
    if let Ok(s) = UnixStream::connect(socket_path).await {
        return Ok(s);
    }
    // Spawn the daemon detached.
    spawn_daemon_detached(socket_path, chrome_executable)?;
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

fn spawn_daemon_detached(socket_path: &Path, chrome_executable: &Path) -> Result<()> {
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
        .arg(socket_path)
        .stdin(std::process::Stdio::null())
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
        } => {
            println!("=== AGENT-BROWSER STATUS ===");
            println!("URL: {url}");
            println!("Viewport: {} x {}", viewport.0, viewport.1);
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
                    }
                }
            }
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
