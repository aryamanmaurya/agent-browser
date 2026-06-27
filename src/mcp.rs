//! MCP (Model Context Protocol) server mode.
//!
//! Runs over stdio as a long-lived JSON-RPC 2.0 server. An MCP client
//! (Claude Desktop, etc.) connects by spawning this process and exchanging
//! newline-delimited JSON messages. Each agent-browser command is exposed
//! as an MCP tool.
//!
//! Key advantage over CLI mode: the `snapshot` tool returns the screenshot
//! as an MCP `image` content block, so multimodal models can actually *see*
//! the page — not just read its text.

use crate::commands::{self, ConsoleBuffer, NetworkBuffer};
use crate::protocol::Response;
use anyhow::{anyhow, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Entry point for `agent-browser mcp`. Launches the browser, then serves
/// MCP requests over stdio until stdin closes. `headful` controls whether
/// the browser window is visible.
pub async fn run(chrome_executable: PathBuf, headful: bool) -> Result<()> {
    tracing::info!("MCP server starting; chrome={:?} headful={}", chrome_executable, headful);

    // Launch browser (same setup as the daemon).
    // MCP mode uses its own user-data-dir to avoid SingletonLock conflicts
    // with any CLI daemons that might be running.
    let user_data_dir = std::env::temp_dir().join("agent-browser-mcp-profile");
    std::fs::create_dir_all(&user_data_dir).ok();
    let mut cfg = BrowserConfig::builder()
        .chrome_executable(chrome_executable)
        .user_data_dir(&user_data_dir)
        .arg("--no-sandbox")
        .arg("--disable-dev-shm-usage");
    if !headful {
        cfg = cfg.arg("--headless=new").arg("--disable-gpu");
    }
    let config = cfg
        .window_size(1280, 800)
        .build()
        .map_err(|e| anyhow!("browser config failed: {e}"))?;
    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| anyhow!("browser launch failed: {e}"))?;

    tokio::spawn(async move {
        while handler.next().await.is_some() {}
    });

    let page: Page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| anyhow!("new_page failed: {e}"))?;

    // Set initial viewport.
    if let Ok(params) =
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::builder()
            .width(1280_i64)
            .height(800_i64)
            .device_scale_factor(1.0_f64)
            .mobile(false)
            .build()
    {
        let _ = page.execute(params).await;
    }

    let console_buf: ConsoleBuffer = Arc::new(Mutex::new(Vec::new()));
    let network_buf: NetworkBuffer = Arc::new(Mutex::new(Vec::new()));
    crate::daemon::spawn_console_listener(&page, console_buf.clone());
    crate::daemon::spawn_network_listener(&page, network_buf.clone());

    let state = McpState {
        page,
        console: console_buf,
        network: network_buf,
        headful,
    };

    // Serve stdio.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut stdout = stdout;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // stdin closed — client disconnected.
            tracing::info!("MCP stdin closed, shutting down");
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("invalid JSON on stdin: {e}");
                continue;
            }
        };
        let response = handle_message(&msg, &state).await;
        if let Some(resp) = response {
            let json = serde_json::to_string(&resp)?;
            stdout.write_all(json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    let _ = browser.close().await;
    let _ = browser.wait().await;
    Ok(())
}

struct McpState {
    page: Page,
    console: ConsoleBuffer,
    network: NetworkBuffer,
    headful: bool,
}

// ---------- JSON-RPC types ----------

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

/// Handle a single JSON-RPC message. Returns None for notifications (no
/// response expected), Some(response) for requests.
async fn handle_message(msg: &Value, state: &McpState) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) → no response.
    if id.is_none() {
        match method {
            "notifications/initialized" => {
                tracing::info!("MCP client initialized");
            }
            _ => {
                tracing::debug!("ignoring notification: {method}");
            }
        }
        return None;
    }

    let id = id.unwrap();
    let result = match method {
        "initialize" => handle_initialize(&params),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => handle_tools_call(&params, state).await,
        "shutdown" => Ok(json!({})),
        _ => Err(JsonRpcError {
            code: -32601,
            message: format!("method not found: {method}"),
        }),
    };

    let resp = match result {
        Ok(val) => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(val),
            error: None,
        },
        Err(err) => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(err),
        },
    };
    serde_json::to_value(&resp).ok()
}

fn handle_initialize(params: &Value) -> Result<Value, JsonRpcError> {
    let client_version = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("2024-11-05");
    tracing::info!("MCP initialize: client protocol version {client_version}");
    Ok(json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "agent-browser",
            "version": "0.1.0"
        }
    }))
}

// ---------- Tool definitions ----------

fn handle_tools_list() -> Value {
    json!({
        "tools": [
            tool("navigate", "Navigate the browser to a URL.", schema_obj_opt(&[
                ("url", "string", "The URL to navigate to.", true),
                ("timeout_ms", "number", "Navigation timeout in milliseconds.", false),
            ])),
            tool("snapshot", "Capture a screenshot (base64 PNG) + visible text + console logs + network failures. The screenshot is returned as an image content block that multimodal models can see.", schema_obj_opt(&[
                ("text_only", "boolean", "Skip the screenshot (text + console only). For text-only models.", false),
            ])),
            tool("status", "Show current page URL, viewport size, and daemon uptime.", schema_obj(&[])),
            tool("click", "Click an element. Selector syntax: css=..., text=..., role=button[name=...], xpath=..., or bare CSS.", schema_obj(&[
                ("selector", "string", "The element selector.", true),
            ])),
            tool("type", "Type text into an input element (focuses + types). Uses React-aware value setters.", schema_obj(&[
                ("selector", "string", "The element selector.", true),
                ("text", "string", "The text to type.", true),
            ])),
            tool("press", "Press a keyboard key (e.g. Enter, Escape, Tab, a).", schema_obj(&[
                ("key", "string", "The key name (Enter, Escape, Tab, etc.) or a single character.", true),
            ])),
            tool("scroll", "Scroll the page in a direction.", schema_obj_opt(&[
                ("direction", "string", "up, down, left, or right (default: down).", false),
                ("amount", "number", "Pixels to scroll (default: 600).", false),
            ])),
            tool("hover", "Hover an element (dispatches mouseover/mouseenter/mousemove).", schema_obj(&[
                ("selector", "string", "The element selector.", true),
            ])),
            tool("select", "Select an option in a <select> element.", schema_obj(&[
                ("selector", "string", "The select element selector.", true),
                ("value", "string", "The option value to select.", true),
            ])),
            tool("resize", "Resize the viewport to exact dimensions.", schema_obj(&[
                ("width", "number", "Viewport width in pixels.", true),
                ("height", "number", "Viewport height in pixels.", true),
            ])),
            tool("viewport", "Set viewport to a preset: mobile (375x667), tablet (768x1024), or desktop (1280x800).", schema_obj_opt(&[
                ("preset", "string", "mobile, tablet, or desktop (default: desktop).", false),
            ])),
            tool("wait_for", "Wait until a condition is met: selector visible, text present, or URL matches.", schema_obj_opt(&[
                ("kind", "string", "selector, text, or url (default: selector).", false),
                ("target", "string", "The selector, text to find, or URL substring.", true),
                ("timeout_ms", "number", "Timeout in milliseconds (default: 5000).", false),
            ])),
            tool("console", "Dump console logs captured since the last snapshot or console drain.", schema_obj(&[])),
            tool("cookies", "Get all cookies for the current page.", schema_obj(&[])),
            tool("local_storage", "Get localStorage entries (all, or a specific key).", schema_obj_opt(&[
                ("key", "string", "Specific key to fetch. If omitted, returns all entries.", false),
            ])),
            tool("network", "Dump all network requests (success + failure) captured since the last drain. Each entry includes method, URL, status, MIME type, size, and failure info.", schema_obj_opt(&[
                ("filter", "string", "Filter requests by URL substring (e.g. /api/).", false),
            ])),
            tool("back", "Go back in browser history.", schema_obj(&[])),
            tool("forward", "Go forward in browser history.", schema_obj(&[])),
            tool("reload", "Reload the current page.", schema_obj(&[])),
            tool("close", "Close the browser session and stop the MCP server.", schema_obj(&[])),
            // New feature tools:
            tool("throttle", "Throttle network conditions. Presets: offline, slow-3g, fast-3g, none.", schema_obj(&[
                ("preset", "string", "offline, slow-3g, fast-3g, or none.", true),
            ])),
            tool("pdf", "Print the page to a PDF file.", schema_obj(&[
                ("path", "string", "File path to save the PDF to.", true),
            ])),
            tool("inspect", "Inspect an element: tag, attributes, computed styles, bounding box, ARIA role/label.", schema_obj(&[
                ("selector", "string", "The element selector.", true),
            ])),
            tool("accessibility", "Get the accessibility tree as indented text.", schema_obj(&[])),
            tool("har", "Export captured network requests as HAR 1.2 JSON to a file.", schema_obj(&[
                ("path", "string", "File path to save the HAR to.", true),
            ])),
        ]
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn schema_obj(required: &[(&str, &str, &str, bool)]) -> Value {
    let mut props = serde_json::Map::new();
    let mut req = Vec::new();
    for (name, ty, desc, is_required) in required {
        props.insert(
            name.to_string(),
            json!({"type": ty, "description": desc}),
        );
        if *is_required {
            req.push(name.to_string());
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": req,
    })
}

/// Same as schema_obj but allows optional fields (none required by default).
fn schema_obj_opt(fields: &[(&str, &str, &str, bool)]) -> Value {
    let mut props = serde_json::Map::new();
    let mut req = Vec::new();
    for (name, ty, desc, is_required) in fields {
        props.insert(
            name.to_string(),
            json!({"type": ty, "description": desc}),
        );
        if *is_required {
            req.push(name.to_string());
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": req,
    })
}

// ---------- Tool dispatch ----------

#[derive(Deserialize)]
struct ToolCallParams {
    name: String,
    arguments: Option<Value>,
}

async fn handle_tools_call(params: &Value, state: &McpState) -> Result<Value, JsonRpcError> {
    let call: ToolCallParams = serde_json::from_value(params.clone()).map_err(|e| JsonRpcError {
        code: -32602,
        message: format!("invalid tools/call params: {e}"),
    })?;

    let args = call.arguments.unwrap_or(json!({}));
    let page = &state.page;

    let resp: Response = match call.name.as_str() {
        "navigate" => {
            let url = get_str(&args, "url")?;
            let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_u64());
            commands::navigate(page, &url, timeout_ms).await
        }
        "snapshot" => {
            let text_only = args.get("text_only").and_then(|v| v.as_bool()).unwrap_or(false);
            commands::snapshot(page, &state.console, &state.network, text_only).await
        }
        "status" => {
            let url = commands::page_url(page).await.unwrap_or_default();
            let viewport = commands::page_viewport(page).await.unwrap_or((1280, 800));
            Response::Status {
                url,
                viewport,
                chrome_pid: std::process::id(),
                uptime_secs: 0,
                headful: state.headful,
            }
        }
        "click" => {
            let selector = get_str(&args, "selector")?;
            commands::click(page, &selector).await
        }
        "type" => {
            let selector = get_str(&args, "selector")?;
            let text = get_str(&args, "text")?;
            commands::type_text(page, &selector, &text).await
        }
        "press" => {
            let key = get_str(&args, "key")?;
            commands::press(page, &key).await
        }
        "scroll" => {
            let direction = args
                .get("direction")
                .and_then(|v| v.as_str())
                .unwrap_or("down")
                .to_string();
            let amount = args.get("amount").and_then(|v| v.as_i64()).map(|i| i as i32);
            commands::scroll(page, &direction, amount).await
        }
        "hover" => {
            let selector = get_str(&args, "selector")?;
            commands::hover(page, &selector).await
        }
        "select" => {
            let selector = get_str(&args, "selector")?;
            let value = get_str(&args, "value")?;
            commands::select_option(page, &selector, &value).await
        }
        "resize" => {
            let width = get_num(&args, "width")? as u32;
            let height = get_num(&args, "height")? as u32;
            commands::resize(page, width, height).await
        }
        "viewport" => {
            let preset = args
                .get("preset")
                .and_then(|v| v.as_str())
                .unwrap_or("desktop")
                .to_string();
            commands::viewport_preset(page, &preset).await
        }
        "wait_for" => {
            let kind = args
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("selector")
                .to_string();
            let target = get_str(&args, "target")?;
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(5000) as u64;
            commands::wait_for(page, &kind, &target, timeout_ms).await
        }
        "console" => commands::console(&state.console).await,
        "cookies" => commands::cookies(page).await,
        "local_storage" => {
            let key = args.get("key").and_then(|v| v.as_str()).map(|s| s.to_string());
            commands::local_storage(page, key.as_deref()).await
        }
        "network" => {
            let filter = args.get("filter").and_then(|v| v.as_str()).map(|s| s.to_string());
            commands::network(&state.network, filter.as_deref()).await
        }
        "back" => commands::back(page).await,
        "forward" => commands::forward(page).await,
        "reload" => commands::reload(page).await,
        "close" => Response::ok("closing session"),
        // New feature tools:
        "throttle" => {
            let preset = get_str(&args, "preset")?;
            commands::throttle(page, &preset).await
        }
        "pdf" => {
            let path = get_str(&args, "path")?;
            commands::pdf(page, &path).await
        }
        "inspect" => {
            let selector = get_str(&args, "selector")?;
            commands::inspect(page, &selector).await
        }
        "accessibility" => commands::accessibility(page).await,
        "har" => {
            let path = get_str(&args, "path")?;
            commands::har(&state.network, &path).await
        }
        _ => {
            return Err(JsonRpcError {
                code: -32602,
                message: format!("unknown tool: {}", call.name),
            })
        }
    };

    let (content, is_error) = response_to_mcp_content(&resp);
    Ok(json!({
        "content": content,
        "isError": is_error,
    }))
}

fn get_str(args: &Value, key: &str) -> Result<String, JsonRpcError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| JsonRpcError {
            code: -32602,
            message: format!("missing required string argument: {key}"),
        })
}

fn get_num(args: &Value, key: &str) -> Result<i64, JsonRpcError> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| JsonRpcError {
            code: -32602,
            message: format!("missing required number argument: {key}"),
        })
}

/// Convert a Response into MCP content blocks (text + optional image).
/// Returns (content_blocks, is_error).
fn response_to_mcp_content(resp: &Response) -> (Vec<Value>, bool) {
    match resp {
        Response::Error { message } => (
            vec![json!({"type": "text", "text": format!("Error: {message}")})],
            true,
        ),
        Response::Ok { message } => (
            vec![json!({"type": "text", "text": message.clone()})],
            false,
        ),
        Response::Status {
            url,
            viewport,
            chrome_pid,
            uptime_secs,
            headful: _,
        } => (
            vec![json!({"type": "text", "text": format!(
                "URL: {url}\nViewport: {} x {}\nPID: {chrome_pid}\nUptime: {uptime_secs}s",
                viewport.0, viewport.1
            )})],
            false,
        ),
        Response::Snapshot {
            screenshot_b64,
            text,
            console,
            network_failures,
            viewport,
            url,
            title,
        } => {
            let mut text_out = String::new();
            text_out.push_str(&format!("URL: {url}\n"));
            text_out.push_str(&format!("Title: {title}\n"));
            text_out.push_str(&format!("Viewport: {} x {}\n", viewport.0, viewport.1));

            if !console.is_empty() {
                text_out.push_str(&format!("\nConsole ({} entries):\n", console.len()));
                for e in console {
                    text_out.push_str(&format!("  [{}] {} ({})\n", e.level, e.text, e.source));
                }
            }

            if !network_failures.is_empty() {
                text_out.push_str(&format!("\nNetwork failures ({}):\n", network_failures.len()));
                for n in network_failures {
                    let err = n.error_text.as_deref().unwrap_or("unknown");
                    text_out.push_str(&format!("  {} {} → FAILED ({})\n", n.method, n.url, err));
                }
            }

            text_out.push_str("\nVisible text:\n");
            text_out.push_str(&trim_text(text, 6000));

            let mut content = vec![json!({"type": "text", "text": text_out})];

            // Add the screenshot as an image block — this is the key MCP
            // advantage: multimodal models can SEE the page.
            if let Some(b64) = screenshot_b64 {
                content.push(json!({
                    "type": "image",
                    "data": b64,
                    "mimeType": "image/png"
                }));
            }

            (content, false)
        }
        Response::Console { entries } => {
            if entries.is_empty() {
                return (vec![json!({"type": "text", "text": "Console is empty."})], false);
            }
            let mut text = String::new();
            for e in entries {
                text.push_str(&format!("[{}] {} ({})\n", e.level, e.text, e.source));
            }
            (vec![json!({"type": "text", "text": text})], false)
        }
        Response::Cookies { cookies } => {
            if cookies.is_empty() {
                return (vec![json!({"type": "text", "text": "No cookies."})], false);
            }
            let mut text = String::new();
            for c in cookies {
                text.push_str(&format!(
                    "{}={} (domain={}, path={}, secure={}, http_only={})\n",
                    c.name, c.value, c.domain, c.path, c.secure, c.http_only
                ));
            }
            (vec![json!({"type": "text", "text": text})], false)
        }
        Response::LocalStorage { entries } => {
            if entries.is_empty() {
                return (vec![json!({"type": "text", "text": "localStorage is empty."})], false);
            }
            let mut text = String::new();
            for (k, v) in entries {
                text.push_str(&format!("{k} = {v}\n"));
            }
            (vec![json!({"type": "text", "text": text})], false)
        }
        Response::Network { entries } => {
            if entries.is_empty() {
                return (vec![json!({"type": "text", "text": "No network requests captured."})], false);
            }
            let mut text = format!("{} requests:\n", entries.len());
            for n in entries {
                if n.failed {
                    let err = n.error_text.as_deref().unwrap_or("unknown");
                    text.push_str(&format!(
                        "  {} {} → FAILED ({}) [{}]\n",
                        n.method, n.url, err, n.resource_type
                    ));
                } else {
                    let status = n.status.unwrap_or(0);
                    let mime = n.mime_type.as_deref().unwrap_or("");
                    let size = n
                        .encoded_size
                        .map(|s| format!("{s}B"))
                        .unwrap_or_else(|| "?".to_string());
                    text.push_str(&format!(
                        "  {} {} → {} ({} {}) [{}]\n",
                        n.method, n.url, status, mime, size, n.resource_type
                    ));
                }
            }
            (vec![json!({"type": "text", "text": text})], false)
        }
        Response::Pdf { path, size_bytes } => (
            vec![json!({"type": "text", "text": format!("PDF saved to {path} ({size_bytes} bytes)")})],
            false,
        ),
        Response::Inspect { info } => {
            let mut text = format!("<{}>\n", info.tag);
            for (k, v) in &info.attributes {
                text.push_str(&format!("  {k}=\"{v}\"\n"));
            }
            if !info.text.is_empty() {
                text.push_str(&format!("Text: {}\n", info.text));
            }
            let bb = info.bounding_box;
            text.push_str(&format!("BBox: x={:.0} y={:.0} w={:.0} h={:.0}\n", bb.0, bb.1, bb.2, bb.3));
            if let Some(r) = &info.aria_role { text.push_str(&format!("Role: {r}\n")); }
            if let Some(l) = &info.aria_label { text.push_str(&format!("Label: {l}\n")); }
            text.push_str("Styles:\n");
            for (k, v) in &info.computed_styles {
                text.push_str(&format!("  {k}: {v}\n"));
            }
            (vec![json!({"type": "text", "text": text})], false)
        }
        Response::Accessibility { tree } => (
            vec![json!({"type": "text", "text": if tree.is_empty() { "(empty accessibility tree)".to_string() } else { tree.clone() }})],
            false,
        ),
        Response::Har { path, size_bytes, request_count } => (
            vec![json!({"type": "text", "text": format!("HAR exported to {path} ({size_bytes} bytes, {request_count} requests)")})],
            false,
        ),
        Response::TabList { tabs, active_id } => {
            let mut text = format!("Tabs ({}):\n", tabs.len());
            for t in tabs {
                let marker = if t.active { "*" } else { " " };
                text.push_str(&format!("{marker} {} {} {}\n", t.id, t.url, t.title));
            }
            text.push_str(&format!("Active: {active_id}\n"));
            (vec![json!({"type": "text", "text": text})], false)
        }
        Response::TabId { id, url } => (
            vec![json!({"type": "text", "text": format!("Tab created: {id} ({url})")})],
            false,
        ),
    }
}

fn trim_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s[..s
        .char_indices()
        .take(max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(max)]
    .to_string();
    out.push_str("\n... [truncated]");
    out
}
