//! The persistent daemon.
//!
//! Holds a chromiumoxide `Browser` + `Page` open and serves `Request`s over a
//! Unix domain socket. The CLI auto-spawns this on first use; it shuts itself
//! down on an idle timeout or an explicit `Shutdown`/`Close` command.

use crate::commands::{self, ConsoleBuffer, NetworkBuffer};
use crate::protocol::{read_frame, write_frame, ConsoleEntry, NetworkEntry, Request, Response, TabInfo};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UnixListener;

pub struct DaemonOptions {
    pub chrome_executable: PathBuf,
    pub socket_path: PathBuf,
    pub initial_url: Option<String>,
    pub idle_timeout_secs: u64,
    pub headful: bool,
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
    // Each session gets its own user-data-dir to avoid Chromium's SingletonLock
    // (which prevents multiple instances from sharing a profile directory).
    let user_data_dir = opts
        .socket_path
        .with_file_name(format!(
            "{}-profile",
            opts.socket_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("default")
        ));
    std::fs::create_dir_all(&user_data_dir).ok();
    let mut browser_cfg = BrowserConfig::builder()
        .chrome_executable(opts.chrome_executable.clone())
        .user_data_dir(&user_data_dir)
        .arg("--no-sandbox")
        .arg("--disable-dev-shm-usage");
    // Headless is the default; only skip it when --headful was requested.
    if !opts.headful {
        browser_cfg = browser_cfg
            .arg("--headless=new")
            .arg("--disable-gpu");
    }
    browser_cfg = browser_cfg.window_size(1280, 800);
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

    let initial_page: Page = browser
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
        let _ = initial_page.execute(params).await;
    }

    let console_buf: ConsoleBuffer = Arc::new(Mutex::new(Vec::new()));
    let network_buf: NetworkBuffer = Arc::new(Mutex::new(Vec::new()));
    spawn_console_listener(&initial_page, console_buf.clone());
    spawn_network_listener(&initial_page, network_buf.clone());

    // Multi-tab manager: maps tab_id → Page, tracks the active tab.
    let tabs: Arc<Mutex<HashMap<String, Page>>> = Arc::new(Mutex::new(HashMap::new()));
    let active_tab: Arc<Mutex<String>> = Arc::new(Mutex::new("tab-1".to_string()));
    tabs.lock().unwrap().insert("tab-1".to_string(), initial_page.clone());

    if let Some(url) = &opts.initial_url {
        let _ = initial_page.goto(url).await;
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

        let resp = dispatch(&req, &tabs, &active_tab, &console_buf, &network_buf, &browser, start, opts.headful).await;
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
    tabs: &Arc<Mutex<HashMap<String, Page>>>,
    active_tab: &Arc<Mutex<String>>,
    console: &ConsoleBuffer,
    network: &NetworkBuffer,
    browser: &Browser,
    start: Instant,
    headful: bool,
) -> Response {
    // Handle tab management requests first (they don't need an active page).
    match req {
        Request::TabList => {
            let active = active_tab.lock().unwrap().clone();
            let mut tab_infos: Vec<TabInfo> = Vec::new();
            for (id, page) in tabs.lock().unwrap().iter() {
                let url = commands::page_url(page).await.unwrap_or_default();
                let title = commands::page_title(page).await.unwrap_or_default();
                tab_infos.push(TabInfo {
                    id: id.clone(),
                    url,
                    title,
                    active: id == &active,
                });
            }
            // Sort by tab id for stable ordering.
            tab_infos.sort_by(|a, b| a.id.cmp(&b.id));
            return Response::TabList { tabs: tab_infos, active_id: active };
        }
        Request::TabNew { url } => {
            let id = format!("tab-{}", tabs.lock().unwrap().len() + 1);
            match browser.new_page("about:blank").await {
                Ok(page) => {
                    // Set viewport on the new tab.
                    if let Ok(params) = chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::builder()
                        .width(1280_i64).height(800_i64).device_scale_factor(1.0_f64).mobile(false).build()
                    {
                        let _ = page.execute(params).await;
                    }
                    // Attach listeners to the new tab (shared buffers).
                    spawn_console_listener(&page, console.clone());
                    spawn_network_listener(&page, network.clone());
                    if let Some(u) = url {
                        let _ = page.goto(u).await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    tabs.lock().unwrap().insert(id.clone(), page);
                    *active_tab.lock().unwrap() = id.clone();
                    return Response::TabId { id, url: url.clone().unwrap_or_else(|| "about:blank".to_string()) };
                }
                Err(e) => return Response::err(format!("tab-new failed: {e}")),
            }
        }
        Request::TabSwitch { id } => {
            if tabs.lock().unwrap().contains_key(id) {
                *active_tab.lock().unwrap() = id.clone();
                return Response::ok(format!("switched to tab {id}"));
            }
            return Response::err(format!("tab not found: {id}"));
        }
        Request::TabClose { id } => {
            let mut tabs_guard = tabs.lock().unwrap();
            if let Some(_page) = tabs_guard.remove(id) {
                // page.close() takes ownership and we can't call it on a cloned
                // Page easily; rely on the browser to clean up when the Page is
                // dropped. This is a best-effort close.
                let active = active_tab.lock().unwrap().clone();
                if active == *id {
                    let next = tabs_guard.keys().next().cloned();
                    *active_tab.lock().unwrap() = next.unwrap_or_default();
                }
                return Response::ok(format!("closed tab {id}"));
            }
            return Response::err(format!("tab not found: {id}"));
        }
        _ => {}
    }

    // For all other requests, get the active page.
    let active_id = active_tab.lock().unwrap().clone();
    let page = {
        let tabs_guard = tabs.lock().unwrap();
        tabs_guard.get(&active_id).cloned()
    };
    let page = match page {
        Some(p) => p,
        None => return Response::err(format!("no active tab (active_id={active_id})")),
    };

    match req {
        Request::Status => {
            let url = commands::page_url(&page).await.unwrap_or_default();
            let viewport = commands::page_viewport(&page).await.unwrap_or((1280, 800));
            Response::Status {
                url,
                viewport,
                chrome_pid: std::process::id(),
                uptime_secs: start.elapsed().as_secs(),
                headful: headful,
            }
        }
        Request::Shutdown | Request::Close => Response::ok("shutting down"),
        Request::Navigate { url, timeout_ms } => commands::navigate(&page, url, *timeout_ms).await,
        Request::Snapshot { text_only } => commands::snapshot(&page, console, network, *text_only).await,
        Request::Click { selector } => commands::click(&page, selector).await,
        Request::Hover { selector } => commands::hover(&page, selector).await,
        Request::Type { selector, text } => commands::type_text(&page, selector, text).await,
        Request::Press { key } => commands::press(&page, key).await,
        Request::Scroll { direction, amount } => commands::scroll(&page, direction, *amount).await,
        Request::Select { selector, value } => commands::select_option(&page, selector, value).await,
        Request::Resize { width, height } => commands::resize(&page, *width, *height).await,
        Request::ViewportPreset { preset } => commands::viewport_preset(&page, preset).await,
        Request::WaitFor {
            wait_kind,
            target,
            timeout_ms,
        } => commands::wait_for(&page, wait_kind, target, *timeout_ms).await,
        Request::Console => commands::console(console).await,
        Request::Cookies => commands::cookies(&page).await,
        Request::LocalStorage { key } => commands::local_storage(&page, key.as_deref()).await,
        Request::Network { filter } => commands::network(network, filter.as_deref()).await,
        Request::Back => commands::back(&page).await,
        Request::Forward => commands::forward(&page).await,
        Request::Reload => commands::reload(&page).await,
        // --- new feature commands ---
        Request::Throttle { preset } => commands::throttle(&page, preset).await,
        Request::Pdf { path } => commands::pdf(&page, path).await,
        Request::Inspect { selector } => commands::inspect(&page, selector).await,
        Request::Accessibility => commands::accessibility(&page).await,
        Request::Har { path } => commands::har(network, path).await,
        // Tab management handled above.
        Request::TabList | Request::TabNew { .. } | Request::TabSwitch { .. } | Request::TabClose { .. } => unreachable!(),
    }
}

/// Best-effort network capture. Subscribes to CDP Network domain events
/// (requestWillBeSent, responseReceived, loadingFinished, loadingFailed) and
/// maintains a per-request entry in the shared buffer. The Network domain
/// must be enabled first via `Network.enable`.
pub fn spawn_network_listener(page: &Page, buf: NetworkBuffer) {
    use chromiumoxide::cdp::browser_protocol::network::EnableParams;

    // Enable the Network domain so events start flowing.
    let page_for_enable = page.clone();
    tokio::spawn(async move {
        if let Err(e) = page_for_enable.execute(EnableParams::default()).await {
            tracing::warn!("failed to enable Network domain: {e}");
        }
    });

    let page_clone = page.clone();
    tokio::spawn(async move {
        use chromiumoxide::cdp::browser_protocol::network::{
            EventRequestWillBeSent, EventResponseReceived, EventLoadingFinished,
            EventLoadingFailed,
        };

        // requestWillBeSent → create new entry
        if let Ok(mut events) = page_clone.event_listener::<EventRequestWillBeSent>().await {
            let buf1 = buf.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    let entry = NetworkEntry {
                        request_id: ev.request_id.inner().clone(),
                        method: ev.request.method.clone(),
                        url: ev.request.url.clone(),
                        resource_type: ev
                            .r#type
                            .as_ref()
                            .map(|t| format!("{:?}", t))
                            .unwrap_or_else(|| "Unknown".to_string()),
                        status: None,
                        status_text: None,
                        mime_type: None,
                        encoded_size: None,
                        failed: false,
                        error_text: None,
                        body: None,
                    };
                    commands::push_network_entry(&buf1, entry);
                }
            });
        } else {
            tracing::warn!("failed to subscribe to EventRequestWillBeSent");
        }

        // responseReceived → update status/mime
        if let Ok(mut events) = page_clone.event_listener::<EventResponseReceived>().await {
            let buf2 = buf.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    let rid = ev.request_id.inner().clone();
                    let status = ev.response.status as i32;
                    let status_text = ev.response.status_text.clone();
                    let mime = ev.response.mime_type.clone();
                    commands::update_network_entry(&buf2, &rid, |e| {
                        e.status = Some(status);
                        e.status_text = Some(status_text);
                        e.mime_type = Some(mime);
                    });
                }
            });
        } else {
            tracing::warn!("failed to subscribe to EventResponseReceived");
        }

        // loadingFinished → record size + capture body for small JSON/text
        if let Ok(mut events) = page_clone.event_listener::<EventLoadingFinished>().await {
            let buf3 = buf.clone();
            let page_for_body = page_clone.clone();
            tokio::spawn(async move {
                use chromiumoxide::cdp::browser_protocol::network::GetResponseBodyParams;
                while let Some(ev) = events.next().await {
                    let rid = ev.request_id.inner().clone();
                    let size = ev.encoded_data_length as i64;
                    commands::update_network_entry(&buf3, &rid, |e| {
                        e.encoded_size = Some(size);
                    });
                    // Capture response body for small JSON/text XHR/Fetch responses.
                    // Check if this entry is JSON/text and small enough.
                    let should_capture = {
                        let buf_lock = buf3.lock().unwrap();
                        buf_lock.iter().find(|e| e.request_id == rid).map(|e| {
                            let is_text = e.mime_type.as_deref()
                                .map(|m| m.contains("json") || m.contains("text") || m.contains("javascript"))
                                .unwrap_or(false);
                            let is_xhr = e.resource_type.contains("Fetch") || e.resource_type.contains("Xhr")
                                || e.resource_type.contains("Document");
                            let small = e.encoded_size.map(|s| s < 65_536).unwrap_or(false);
                            is_text && is_xhr && small && !e.failed
                        }).unwrap_or(false)
                    };
                    if should_capture {
                        if let Ok(resp) = page_for_body.execute(GetResponseBodyParams::new(
                            chromiumoxide::cdp::browser_protocol::network::RequestId::new(rid.clone()),
                        )).await {
                            let body = if resp.base64_encoded {
                                match base64::engine::general_purpose::STANDARD.decode(&resp.body) {
                                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                                    Err(_) => format!("<binary {} bytes>", resp.body.len()),
                                }
                            } else {
                                resp.body.clone()
                            };
                            let truncated = if body.len() > 4096 {
                                format!("{}... [truncated]", &body[..4096])
                            } else {
                                body
                            };
                            commands::update_network_entry(&buf3, &rid, |e| {
                                e.body = Some(truncated);
                            });
                        }
                    }
                }
            });
        } else {
            tracing::warn!("failed to subscribe to EventLoadingFinished");
        }

        // loadingFailed → mark as failed + error text
        if let Ok(mut events) = page_clone.event_listener::<EventLoadingFailed>().await {
            let buf4 = buf.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    let rid = ev.request_id.inner().clone();
                    let err = ev.error_text.clone();
                    commands::update_network_entry(&buf4, &rid, |e| {
                        e.failed = true;
                        e.error_text = Some(err);
                    });
                }
            });
        } else {
            tracing::warn!("failed to subscribe to EventLoadingFailed");
        }
    });
}

/// Best-effort console capture. Subscribes to CDP `Runtime.consoleAPICalled`
/// and `Runtime.exceptionThrown` events and pushes entries into the shared
/// buffer. If subscription fails the daemon still works, just without console.
pub fn spawn_console_listener(page: &Page, buf: ConsoleBuffer) {
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
