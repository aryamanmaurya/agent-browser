//! Command implementations against the chromiumoxide Page.
//!
//! Each public function takes the live `Page` (and console buffer where
//! relevant) and returns a `Response`. The daemon dispatches to these.

use crate::protocol::{ConsoleEntry, Cookie, NetworkEntry, Response};
use crate::selector::{js_quote, resolve, Resolved};
use anyhow::Result;
use serde_json::json;
use base64::Engine;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::Page;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared console log buffer, drained on snapshot/console commands.
pub type ConsoleBuffer = Arc<Mutex<Vec<ConsoleEntry>>>;

/// Shared network request buffer. Stores all requests (success + failure).
/// Drained on the `network` command; peeked (not drained) for snapshot failures.
pub type NetworkBuffer = Arc<Mutex<Vec<NetworkEntry>>>;

/// Cap the network buffer to prevent unbounded growth on heavy pages.
const NETWORK_BUFFER_CAP: usize = 1000;

pub async fn navigate(page: &Page, url: &str, timeout_ms: Option<u64>) -> Response {
    let goto_fut = page.goto(url);
    let result = match timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), goto_fut).await,
        None => Ok(goto_fut.await),
    };
    match result {
        Ok(Ok(_)) => {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = page.evaluate("(() => null)").await;
            Response::ok(format!("navigated to {}", url))
        }
        Ok(Err(e)) => Response::err(format!("navigate failed: {e}")),
        Err(_) => Response::err(format!("navigate timed out after {}ms", timeout_ms.unwrap_or(0))),
    }
}

pub async fn snapshot(
    page: &Page,
    console: &ConsoleBuffer,
    network: &NetworkBuffer,
    text_only: bool,
) -> Response {
    let url = page_url(page).await.unwrap_or_default();
    let title = page_title(page).await.unwrap_or_default();
    let text = page_text(page).await.unwrap_or_default();
    let viewport = page_viewport(page).await.unwrap_or((1280, 800));
    let console_entries = drain_console(console);
    // Peek (clone without clearing) failed requests for the snapshot.
    // Full network log is drained by the `network` command.
    let network_failures = {
        let buf = network.lock().unwrap();
        buf.iter().filter(|e| e.failed).cloned().collect()
    };

    let screenshot_b64 = if text_only {
        None
    } else {
        match page
            .screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .build(),
            )
            .await
        {
            Ok(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            Err(e) => {
                tracing::warn!("screenshot failed: {e}");
                None
            }
        }
    };

    Response::Snapshot {
        screenshot_b64,
        text,
        console: console_entries,
        network_failures,
        viewport,
        url,
        title,
    }
}

pub async fn click(page: &Page, selector: &str) -> Response {
    match resolve(selector) {
        Resolved::Css(css) => match page.find_element(&css).await {
            Ok(el) => match el.click().await {
                Ok(_) => Response::ok("clicked"),
                Err(e) => Response::err(format!("click failed: {e}")),
            },
            Err(e) => Response::err(format!("element not found (css={css}): {e}")),
        },
        Resolved::Js(expr) => {
            let script = format!(
                "(() => {{ const el = {expr}; if (!el) throw new Error('element not found'); el.click(); }})()"
            );
            run_js(page, &script, "click").await
        }
    }
}

pub async fn hover(page: &Page, selector: &str) -> Response {
    let script = match resolve(selector) {
        Resolved::Css(css) => format!(
            "(() => {{ const el = document.querySelector({q}); if (!el) throw new Error('element not found'); const ev = (t) => new MouseEvent(t, {{bubbles:true}}); el.dispatchEvent(ev('mouseover')); el.dispatchEvent(ev('mouseenter')); el.dispatchEvent(ev('mousemove')); }})()",
            q = js_quote(&css)
        ),
        Resolved::Js(expr) => format!(
            "(() => {{ const el = {expr}; if (!el) throw new Error('element not found'); const ev = (t) => new MouseEvent(t, {{bubbles:true}}); el.dispatchEvent(ev('mouseover')); el.dispatchEvent(ev('mouseenter')); el.dispatchEvent(ev('mousemove')); }})()"
        ),
    };
    run_js(page, &script, "hover").await
}

pub async fn type_text(page: &Page, selector: &str, text: &str) -> Response {
    match resolve(selector) {
        Resolved::Css(css) => match page.find_element(&css).await {
            Ok(el) => {
                // Focus then type. type_str simulates real keypresses which
                // works with React/Vue controlled inputs.
                let _ = el.click().await; // focus
                match el.type_str(text).await {
                    Ok(_) => Response::ok("typed"),
                    Err(e) => Response::err(format!("type failed: {e}")),
                }
            }
            Err(e) => Response::err(format!("element not found (css={css}): {e}")),
        },
        Resolved::Js(expr) => {
            // React-aware native value setter so controlled inputs update.
            let script = format!(
                "(() => {{\
                   const el = {expr};\
                   if (!el) throw new Error('element not found');\
                   el.focus();\
                   const proto = Object.getPrototypeOf(el);\
                   const desc = Object.getOwnPropertyDescriptor(proto, 'value');\
                   if (desc && desc.set) {{ desc.set.call(el, {val}); }}\
                   else {{ el.value = {val}; }}\
                   el.dispatchEvent(new Event('input', {{bubbles: true}}));\
                   el.dispatchEvent(new Event('change', {{bubbles: true}}));\
                 }})()",
                val = js_quote(text)
            );
            run_js(page, &script, "type").await
        }
    }
}

pub async fn press(page: &Page, key: &str) -> Response {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };
    // Build the params: 1 for a printable char, 2 (down+up) for a named key.
    let (p1, p2_opt) = if key.chars().count() == 1 {
        let p = match DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::Char)
            .text(key)
            .build()
        {
            Ok(p) => p,
            Err(e) => return Response::err(format!("press param error: {e}")),
        };
        (p, None)
    } else {
        let down = match DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(key)
            .code(key)
            .build()
        {
            Ok(p) => p,
            Err(e) => return Response::err(format!("press param error: {e}")),
        };
        let up = match DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(key)
            .code(key)
            .build()
        {
            Ok(p) => p,
            Err(e) => return Response::err(format!("press param error: {e}")),
        };
        (down, Some(up))
    };
    if let Err(e) = page.execute(p1).await {
        return Response::err(format!("press failed: {e}"));
    }
    if let Some(p2) = p2_opt {
        if let Err(e) = page.execute(p2).await {
            return Response::err(format!("press failed: {e}"));
        }
    }
    Response::ok(format!("pressed {key}"))
}

pub async fn scroll(page: &Page, direction: &str, amount: Option<i32>) -> Response {
    let amt = amount.unwrap_or(600);
    let (dx, dy) = match direction {
        "up" => (0, -amt),
        "down" => (0, amt),
        "left" => (-amt, 0),
        "right" => (amt, 0),
        other => return Response::err(format!("unknown scroll direction: {other}")),
    };
    let script = format!("window.scrollBy({dx}, {dy})");
    run_js(page, &script, "scroll").await
}

pub async fn select_option(page: &Page, selector: &str, value: &str) -> Response {
    let expr = match resolve(selector) {
        Resolved::Css(css) => format!("document.querySelector({})", js_quote(&css)),
        Resolved::Js(expr) => expr,
    };
    let script = format!(
        "(() => {{\
           const el = {expr};\
           if (!el) throw new Error('element not found');\
           if (el.tagName !== 'SELECT') throw new Error('element is not a <select>');\
           const proto = Object.getPrototypeOf(el);\
           const desc = Object.getOwnPropertyDescriptor(proto, 'value');\
           if (desc && desc.set) {{ desc.set.call(el, {val}); }}\
           else {{ el.value = {val}; }}\
           el.dispatchEvent(new Event('input', {{bubbles: true}}));\
           el.dispatchEvent(new Event('change', {{bubbles: true}}));\
         }})()",
        val = js_quote(value)
    );
    run_js(page, &script, "select").await
}

pub async fn resize(page: &Page, width: u32, height: u32) -> Response {
    use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
    let params = match SetDeviceMetricsOverrideParams::builder()
        .width(width as i64)
        .height(height as i64)
        .device_scale_factor(1.0_f64)
        .mobile(false)
        .build()
    {
        Ok(p) => p,
        Err(e) => return Response::err(format!("invalid viewport params: {e}")),
    };
    match page.execute(params).await {
        Ok(_) => Response::ok(format!("viewport set to {width}x{height}")),
        Err(e) => Response::err(format!("resize failed: {e}")),
    }
}

pub async fn viewport_preset(page: &Page, preset: &str) -> Response {
    let (w, h) = match preset {
        "mobile" => (375, 667),
        "tablet" => (768, 1024),
        "desktop" => (1280, 800),
        other => return Response::err(format!("unknown preset: {other}")),
    };
    resize(page, w, h).await
}

pub async fn wait_for(page: &Page, kind: &str, target: &str, timeout_ms: u64) -> Response {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let met = check_condition(page, kind, target).await;
        if met {
            return Response::ok(format!("wait condition met: {kind}={target}"));
        }
        if Instant::now() >= deadline {
            return Response::err(format!("wait timed out after {timeout_ms}ms: {kind}={target}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn check_condition(page: &Page, kind: &str, target: &str) -> bool {
    let expr = match kind {
        "selector" => match resolve(target) {
            Resolved::Css(c) => format!("!!document.querySelector({})", js_quote(&c)),
            Resolved::Js(e) => format!("!!({})", e),
        },
        "text" => format!(
            "(document.body && (document.body.innerText || '').includes({}))",
            js_quote(target)
        ),
        "url" => format!("location.href.includes({})", js_quote(target)),
        _ => return false,
    };
    match page.evaluate(expr.as_str()).await {
        Ok(v) => v.into_value::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}

pub async fn console(console: &ConsoleBuffer) -> Response {
    Response::Console { entries: drain_console(console) }
}

pub async fn network(network: &NetworkBuffer, filter: Option<&str>) -> Response {
    let entries = {
        let mut buf = network.lock().unwrap();
        let drained = buf.clone();
        buf.clear();
        drained
    };
    let filtered = match filter {
        Some(f) => entries.into_iter().filter(|e| e.url.contains(f)).collect(),
        None => entries,
    };
    Response::Network { entries: filtered }
}

pub async fn cookies(page: &Page) -> Response {
    match page.get_cookies().await {
        Ok(cookies) => {
            let out: Vec<Cookie> = cookies
                .into_iter()
                .map(|c| Cookie {
                    name: c.name,
                    value: c.value,
                    domain: c.domain,
                    path: c.path,
                    secure: c.secure,
                    http_only: c.http_only,
                })
                .collect();
            Response::Cookies { cookies: out }
        }
        Err(e) => Response::err(format!("cookies failed: {e}")),
    }
}

pub async fn local_storage(page: &Page, key: Option<&str>) -> Response {
    let expr = match key {
        Some(k) => format!(
            "JSON.stringify([[{q}, localStorage.getItem({q})]])",
            q = js_quote(k)
        ),
        None => "JSON.stringify(Object.entries(localStorage))".to_string(),
    };
    match page.evaluate(expr.as_str()).await {
        Ok(v) => {
            let json: String = v.into_value().unwrap_or_else(|_| "[]".to_string());
            let pairs: Vec<(String, String)> = serde_json::from_str(&json).unwrap_or_default();
            Response::LocalStorage { entries: pairs }
        }
        Err(e) => Response::err(format!("localStorage failed: {e}")),
    }
}

pub async fn back(page: &Page) -> Response {
    // No native page.go_back() in chromiumoxide 0.7; use history.back().
    match page.evaluate("history.back()").await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Response::ok("back")
        }
        Err(e) => {
            // history.back() navigates the page, which tears down the JS
            // execution context and makes evaluate reject with
            // "Inspected target navigated or closed". The navigation DID
            // happen, so treat this as success.
            let msg = format!("{e}");
            if msg.contains("navigated") || msg.contains("closed") {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Response::ok("back")
            } else {
                Response::err(format!("back failed: {e}"))
            }
        }
    }
}

pub async fn forward(page: &Page) -> Response {
    match page.evaluate("history.forward()").await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Response::ok("forward")
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("navigated") || msg.contains("closed") {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Response::ok("forward")
            } else {
                Response::err(format!("forward failed: {e}"))
            }
        }
    }
}

pub async fn reload(page: &Page) -> Response {
    match page.reload().await {
        Ok(_) => Response::ok("reloaded"),
        Err(e) => Response::err(format!("reload failed: {e}")),
    }
}

// ---------- helpers ----------

pub async fn page_url(page: &Page) -> Result<String> {
    Ok(page
        .evaluate("location.href")
        .await?
        .into_value::<String>()?)
}

pub async fn page_title(page: &Page) -> Result<String> {
    Ok(page
        .evaluate("document.title")
        .await?
        .into_value::<String>()?)
}

pub async fn page_text(page: &Page) -> Result<String> {
    Ok(page
        .evaluate("document.body ? (document.body.innerText || '') : ''")
        .await?
        .into_value::<String>()?)
}

pub async fn page_viewport(page: &Page) -> Result<(u32, u32)> {
    let script = "JSON.stringify([window.innerWidth, window.innerHeight])";
    let json: String = page.evaluate(script).await?.into_value()?;
    let v: (u32, u32) = serde_json::from_str(&json)?;
    Ok(v)
}

fn drain_console(console: &ConsoleBuffer) -> Vec<ConsoleEntry> {
    let mut buf = console.lock().unwrap();
    let drained = buf.clone();
    buf.clear();
    drained
}

/// Push a network entry into the buffer, capping at NETWORK_BUFFER_CAP (drop oldest).
pub fn push_network_entry(network: &NetworkBuffer, entry: NetworkEntry) {
    let mut buf = network.lock().unwrap();
    if buf.len() >= NETWORK_BUFFER_CAP {
        buf.remove(0);
    }
    buf.push(entry);
}

/// Update an existing network entry by request_id (for response/finish/fail events).
pub fn update_network_entry<F>(network: &NetworkBuffer, request_id: &str, update: F)
where
    F: FnOnce(&mut NetworkEntry),
{
    let mut buf = network.lock().unwrap();
    for entry in buf.iter_mut() {
        if entry.request_id == request_id {
            update(entry);
            return;
        }
    }
}

async fn run_js(page: &Page, script: &str, label: &str) -> Response {
    match page.evaluate(script).await {
        Ok(_) => Response::ok(label.to_string()),
        Err(e) => Response::err(format!("{label} failed: {e}")),
    }
}

// ==================== NEW FEATURE COMMANDS ====================

/// Throttle network conditions. Presets: offline, slow-3g, fast-3g, none.
pub async fn throttle(page: &Page, preset: &str) -> Response {
    use chromiumoxide::cdp::browser_protocol::network::EmulateNetworkConditionsParams;
    let (offline, latency, dl, ul) = match preset {
        "offline" => (true, 0.0, 0.0, 0.0),
        "slow-3g" => (false, 400.0, 50_000.0, 20_000.0),  // ~400ms latency, 50KB/s down
        "fast-3g" => (false, 150.0, 250_000.0, 100_000.0), // ~150ms, 250KB/s down
        "none" => (false, 0.0, -1.0, -1.0), // -1 disables throttling
        other => return Response::err(format!("unknown throttle preset: {other} (use: offline, slow-3g, fast-3g, none)")),
    };
    match EmulateNetworkConditionsParams::builder()
        .offline(offline)
        .latency(latency)
        .download_throughput(dl)
        .upload_throughput(ul)
        .build()
    {
        Ok(params) => match page.execute(params).await {
            Ok(_) => Response::ok(format!("network throttled: {preset}")),
            Err(e) => Response::err(format!("throttle failed: {e}")),
        },
        Err(e) => Response::err(format!("throttle params error: {e}")),
    }
}

/// Print the page to PDF and save to path.
pub async fn pdf(page: &Page, path: &str) -> Response {
    use chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams;
    let params = PrintToPdfParams::builder()
        .print_background(true)
        .build();
    match page.pdf(params).await {
        Ok(bytes) => {
            match std::fs::write(path, &bytes) {
                Ok(_) => Response::Pdf { path: path.to_string(), size_bytes: bytes.len() },
                Err(e) => Response::err(format!("failed to write PDF: {e}")),
            }
        }
        Err(e) => Response::err(format!("PDF generation failed: {e}")),
    }
}

/// Inspect an element: tag, attributes, computed styles, bounding box, text, ARIA.
pub async fn inspect(page: &Page, selector: &str) -> Response {
    let expr = match resolve(selector) {
        Resolved::Css(css) => format!("document.querySelector({})", js_quote(&css)),
        Resolved::Js(expr) => expr,
    };
    // Serialize element info via JS, then return as ElementInfo.
    let script = format!(
        "(() => {{\
           const el = {expr};\
           if (!el) throw new Error('element not found');\
           const rect = el.getBoundingClientRect();\
           const cs = window.getComputedStyle(el);\
           const styles = ['display','visibility','position','color','background-color','font-size',\
                           'width','height','margin','padding','border','opacity','z-index']\
             .map(p => [p, cs.getPropertyValue(p)]);\
           const attrs = Array.from(el.attributes).map(a => [a.name, a.value]);\
           return JSON.stringify({{\
             tag: el.tagName,\
             attributes: attrs,\
             text: (el.innerText || '').slice(0, 500),\
             bbox: [rect.x, rect.y, rect.width, rect.height],\
             styles: styles,\
             role: el.getAttribute('role'),\
             label: el.getAttribute('aria-label')\
           }});\
         }})()"
    );
    match page.evaluate(script.as_str()).await {
        Ok(v) => {
            let json: String = v.into_value().unwrap_or_default();
            match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(val) => {
                    let tag = val["tag"].as_str().unwrap_or("").to_string();
                    let attributes = val["attributes"].as_array()
                        .map(|arr| arr.iter().filter_map(|a| {
                            let arr = a.as_array()?;
                            Some((arr[0].as_str()?.to_string(), arr[1].as_str()?.to_string()))
                        }).collect())
                        .unwrap_or_default();
                    let text = val["text"].as_str().unwrap_or("").to_string();
                    let bbox = val["bbox"].as_array()
                        .map(|arr| {
                            let f: Vec<f64> = arr.iter().filter_map(|x| x.as_f64()).collect();
                            if f.len() == 4 { (f[0], f[1], f[2], f[3]) } else { (0.0, 0.0, 0.0, 0.0) }
                        })
                        .unwrap_or((0.0, 0.0, 0.0, 0.0));
                    let computed_styles = val["styles"].as_array()
                        .map(|arr| arr.iter().filter_map(|a| {
                            let arr = a.as_array()?;
                            Some((arr[0].as_str()?.to_string(), arr[1].as_str()?.to_string()))
                        }).collect())
                        .unwrap_or_default();
                    let aria_role = val["role"].as_str().map(|s| s.to_string());
                    let aria_label = val["label"].as_str().map(|s| s.to_string());
                    Response::Inspect {
                        info: crate::protocol::ElementInfo {
                            tag,
                            attributes,
                            text,
                            bounding_box: bbox,
                            computed_styles,
                            aria_role,
                            aria_label,
                        },
                    }
                }
                Err(e) => Response::err(format!("inspect parse error: {e}")),
            }
        }
        Err(e) => Response::err(format!("inspect failed: {e}")),
    }
}

/// Get the accessibility tree as indented text.
/// Uses a JS-based approach that walks the DOM and computes ARIA roles,
/// rather than CDP's Accessibility.getFullAXTree (which returns "uninteresting"
/// on some Chrome/chromiumoxide version combos).
pub async fn accessibility(page: &Page) -> Response {
    // JS that walks the DOM and produces a flat list of {role, name, level} entries.
    let script = r#"(() => {
        const result = [];
        const implicitRoles = {
            A: 'link', BUTTON: 'button', INPUT: 'textbox', SELECT: 'listbox',
            TEXTAREA: 'textbox', IMG: 'image', H1: 'heading', H2: 'heading',
            H3: 'heading', H4: 'heading', H5: 'heading', H6: 'heading',
            NAV: 'navigation', MAIN: 'main', ASIDE: 'complementary',
            HEADER: 'banner', FOOTER: 'contentinfo', SECTION: 'region',
            ARTICLE: 'article', FORM: 'form', UL: 'list', OL: 'list', LI: 'listitem',
            TABLE: 'table', TR: 'row', TH: 'columnheader', TD: 'cell',
            FIELDSET: 'group', LEGEND: 'legend', LABEL: 'label',
            DIALOG: 'dialog', DETAILS: 'group', SUMMARY: 'button',
            PROGRESS: 'progressbar', METER: 'meter', CANVAS: 'canvas',
            SVG: 'img', FIGURE: 'figure', FIGCAPTION: 'caption',
            BLOCKQUOTE: 'blockquote', CODE: 'code', TIME: 'time',
        };
        function getRole(el) {
            const explicit = el.getAttribute('role');
            if (explicit) return explicit;
            const tag = el.tagName;
            return implicitRoles[tag] || null;
        }
        function getName(el, role) {
            // ARIA naming order: aria-label, aria-labelledby, text content, title, placeholder
            const ariaLabel = el.getAttribute('aria-label');
            if (ariaLabel) return ariaLabel;
            const title = el.getAttribute('title');
            if (title) return title;
            if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
                const ph = el.getAttribute('placeholder');
                if (ph) return ph;
            }
            const text = (el.innerText || el.textContent || '').trim();
            if (text && text.length < 200) return text;
            return '';
        }
        function walk(el, depth) {
            if (el.nodeType !== 1) return;
            const tag = el.tagName;
            if (['SCRIPT','STYLE','NOSCRIPT','TEMPLATE'].includes(tag)) return;
            const style = window.getComputedStyle(el);
            if (style.display === 'none' || style.visibility === 'hidden') return;
            const role = getRole(el);
            if (role) {
                const name = getName(el, role);
                let entry = { role, name: name, depth: depth };
                if (tag === 'INPUT') {
                    const t = el.getAttribute('type') || 'text';
                    entry.role = t === 'checkbox' ? 'checkbox' : t === 'radio' ? 'radio' : t === 'button' || t === 'submit' ? 'button' : 'textbox';
                    entry.type = t;
                }
                result.push(entry);
            }
            for (const child of el.children) {
                walk(child, depth + (role ? 1 : 0));
            }
        }
        walk(document.body, 0);
        return JSON.stringify(result);
    })()"#;
    match page.evaluate(script).await {
        Ok(v) => {
            let json: String = v.into_value().unwrap_or_else(|_| "[]".to_string());
            let entries: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap_or_default();
            let mut tree = String::new();
            for entry in &entries {
                let role = entry["role"].as_str().unwrap_or("");
                let name = entry["name"].as_str().unwrap_or("");
                let depth = entry["depth"].as_i64().unwrap_or(0) as usize;
                let indent = "  ".repeat(depth);
                if !name.is_empty() {
                    tree.push_str(&format!("{indent}{role}: \"{name}\"\n"));
                } else {
                    tree.push_str(&format!("{indent}{role}\n"));
                }
            }
            Response::Accessibility { tree }
        }
        Err(e) => Response::err(format!("accessibility tree failed: {e}")),
    }
}

/// Export captured network requests as HAR 1.2 JSON to a file.
pub async fn har(network: &NetworkBuffer, path: &str) -> Response {
    let entries = {
        let buf = network.lock().unwrap();
        let drained = buf.clone();
        // NOTE: do NOT clear — HAR export is read-only, unlike `network` command.
        drained
    };

    let har_entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            json!({
                "request": {
                    "method": e.method,
                    "url": e.url,
                    "headers": [],
                },
                "response": {
                    "status": e.status.unwrap_or(0),
                    "statusText": e.status_text.clone().unwrap_or_default(),
                    "content": {
                        "mimeType": e.mime_type.clone().unwrap_or_default(),
                        "size": e.encoded_size.unwrap_or(0),
                        "text": e.body.clone().unwrap_or_default(),
                    },
                },
                "resourceType": e.resource_type,
                "failed": e.failed,
                "error": e.error_text.clone().unwrap_or_default(),
            })
        })
        .collect();

    let har = json!({
        "log": {
            "version": "1.2",
            "creator": {
                "name": "agent-browser",
                "version": "0.2.0"
            },
            "entries": har_entries,
        }
    });

    let json_str = serde_json::to_string_pretty(&har).unwrap_or_default();
    let size = json_str.len();
    match std::fs::write(path, &json_str) {
        Ok(_) => Response::Har {
            path: path.to_string(),
            size_bytes: size,
            request_count: entries.len(),
        },
        Err(e) => Response::err(format!("failed to write HAR: {e}")),
    }
}
