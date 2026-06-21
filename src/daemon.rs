//! The persistent daemon.
//!
//! Holds a chromiumoxide `Browser` + `Page` open and serves `Request`s over a
//! Unix domain socket. The CLI auto-spawns this on first use; it shuts itself
//! down on an idle timeout or an explicit `Shutdown`/`Close` command.

use crate::commands::{self, ConsoleBuffer};
use crate::protocol::{read_frame, write_frame, ConsoleEntry, Request, Response};
use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UnixListener;

pub struct DaemonOptions {
    pub chrome_executable: PathBuf,
    pub socket_path: PathBuf,
    pub initial_url: Option<String>,
    pub idle_timeout_secs: u64,
}

pub async fn run(opts: DaemonOptions) -> Result<()> {
    tracing::info!(
        "daemon starting; chrome={:?} socket={:?}",
        opts.chrome_executable,
        opts.socket_path
    );

    // Prepare the socket path.
    if let Some(parent) = opts.socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::remove_file(&opts.socket_path).ok();

    // Launch the browser.
    let browser_cfg = BrowserConfig::builder()
        .chrome_executable(opts.chrome_executable.clone())
        .arg("--headless=new")
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .window_size(1280, 800);
    let config = browser_cfg
        .build()
        .map_err(|e| anyhow!("browser config failed: {e}"))?;
    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| anyhow!("browser launch failed: {e}"))?;

    // Pump the CDP handler on a background task for the browser's lifetime.
    tokio::spawn(async move {
        while handler.next().await.is_some() {}
        tracing::info!("cdp handler stream ended");
    });

    let page: Page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow!("new_page failed: {e}"))?;

    // Set a sensible initial viewport (Chromium defaults to 800x600 in headless).
    if let Ok(params) = chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::builder()
        .width(1280_i64)
        .height(800_i64)
        .device_scale_factor(1.0_f64)
        .mobile(false)
        .build()
    {
        let _ = page.execute(params).await;
    }

    let console_buf: ConsoleBuffer = Arc::new(Mutex::new(Vec::new()));
    spawn_console_listener(&page, console_buf.clone());

    if let Some(url) = &opts.initial_url {
        let _ = page.goto(url).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let listener = UnixListener::bind(&opts.socket_path)
        .with_context(|| format!("bind socket {:?}", opts.socket_path))?;
    tracing::info!("daemon listening on {:?}", opts.socket_path);

    let start = Instant::now();
    let idle_timeout = Duration::from_secs(opts.idle_timeout_secs);
    let mut idle_deadline = Instant::now() + idle_timeout;

    loop {
        if Instant::now() >= idle_deadline {
            tracing::info!("idle timeout reached, shutting down");
            break;
        }
        let remaining = idle_deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(5));

        let accept = tokio::time::timeout(remaining, listener.accept()).await;
        let (mut stream, _peer) = match accept {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!("accept error: {e}");
                continue;
            }
            Err(_) => continue, // timeout; re-check idle
        };

        let req: Request = match read_frame(&mut stream).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("read request failed: {e}");
                let _ = write_frame(&mut stream, &Response::err(format!("read failed: {e}"))).await;
                continue;
            }
        };

        let resp = dispatch(&req, &page, &console_buf, start).await;
        idle_deadline = Instant::now() + idle_timeout;

        let is_shutdown = matches!(req, Request::Shutdown | Request::Close);
        if let Err(e) = write_frame(&mut stream, &resp).await {
            tracing::error!("failed to write response: {e}");
        }

        if is_shutdown {
            tracing::info!("shutdown requested");
            break;
        }
    }

    tracing::info!("daemon closing browser and removing socket");
    let _ = browser.close().await;
    let _ = browser.wait().await;
    std::fs::remove_file(&opts.socket_path).ok();
    Ok(())
}

async fn dispatch(
    req: &Request,
    page: &Page,
    console: &ConsoleBuffer,
    start: Instant,
) -> Response {
    match req {
        Request::Status => {
            let url = commands::page_url(page).await.unwrap_or_default();
            let viewport = commands::page_viewport(page).await.unwrap_or((1280, 800));
            Response::Status {
                url,
                viewport,
                chrome_pid: std::process::id(),
                uptime_secs: start.elapsed().as_secs(),
            }
        }
        Request::Shutdown | Request::Close => Response::ok("shutting down"),
        Request::Navigate { url } => commands::navigate(page, url).await,
        Request::Snapshot { text_only } => commands::snapshot(page, console, *text_only).await,
        Request::Click { selector } => commands::click(page, selector).await,
        Request::Hover { selector } => commands::hover(page, selector).await,
        Request::Type { selector, text } => commands::type_text(page, selector, text).await,
        Request::Press { key } => commands::press(page, key).await,
        Request::Scroll { direction, amount } => commands::scroll(page, direction, *amount).await,
        Request::Select { selector, value } => commands::select_option(page, selector, value).await,
        Request::Resize { width, height } => commands::resize(page, *width, *height).await,
        Request::ViewportPreset { preset } => commands::viewport_preset(page, preset).await,
        Request::WaitFor {
            wait_kind,
            target,
            timeout_ms,
        } => commands::wait_for(page, wait_kind, target, *timeout_ms).await,
        Request::Console => commands::console(console).await,
        Request::Cookies => commands::cookies(page).await,
        Request::LocalStorage { key } => commands::local_storage(page, key.as_deref()).await,
        Request::Back => commands::back(page).await,
        Request::Forward => commands::forward(page).await,
        Request::Reload => commands::reload(page).await,
    }
}

/// Best-effort console capture. Subscribes to CDP `Runtime.consoleAPICalled`
/// and `Runtime.exceptionThrown` events and pushes entries into the shared
/// buffer. If subscription fails the daemon still works, just without console.
fn spawn_console_listener(page: &Page, buf: ConsoleBuffer) {
    let page_clone = page.clone();
    tokio::spawn(async move {
        use chromiumoxide::cdp::js_protocol::runtime::{
            EventConsoleApiCalled, EventExceptionThrown,
        };

        if let Ok(mut events) = page_clone.event_listener::<EventConsoleApiCalled>().await {
            let buf1 = buf.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    let level = format!("{:?}", ev.r#type).to_lowercase();
                    let text = ev
                        .args
                        .iter()
                        .map(|a| {
                            a.description
                                .clone()
                                .or_else(|| a.value.as_ref().map(|v| v.to_string()))
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    buf1.lock().unwrap().push(ConsoleEntry {
                        level,
                        text,
                        source: "console".to_string(),
                    });
                }
            });
        } else {
            tracing::warn!("failed to subscribe to EventConsoleApiCalled");
        }

        if let Ok(mut events) = page_clone.event_listener::<EventExceptionThrown>().await {
            let buf2 = buf.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    let text = ev
                        .exception_details
                        .exception
                        .as_ref()
                        .and_then(|d| d.description.clone())
                        .unwrap_or_else(|| ev.exception_details.text.clone());
                    buf2.lock().unwrap().push(ConsoleEntry {
                        level: "error".to_string(),
                        text,
                        source: "exception".to_string(),
                    });
                }
            });
        } else {
            tracing::warn!("failed to subscribe to EventExceptionThrown");
        }
    });
}
