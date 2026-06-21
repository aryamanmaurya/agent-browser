//! Command implementations against the chromiumoxide Page.
//!
//! Each public function takes the live `Page` (and console buffer where
//! relevant) and returns a `Response`. The daemon dispatches to these.

use crate::protocol::{ConsoleEntry, Cookie, Response};
use crate::selector::{js_quote, resolve, Resolved};
use anyhow::Result;
use base64::Engine;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::Page;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared console log buffer, drained on snapshot/console commands.
pub type ConsoleBuffer = Arc<Mutex<Vec<ConsoleEntry>>>;

pub async fn navigate(page: &Page, url: &str) -> Response {
    match page.goto(url).await {
        Ok(_) => {
            // Settle: wait a beat for any SPA rendering.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = page.evaluate("(() => null)").await;
            Response::ok(format!("navigated to {}", url))
        }
        Err(e) => Response::err(format!("navigate failed: {e}")),
    }
}

pub async fn snapshot(page: &Page, console: &ConsoleBuffer, text_only: bool) -> Response {
    let url = page_url(page).await.unwrap_or_default();
    let title = page_title(page).await.unwrap_or_default();
    let text = page_text(page).await.unwrap_or_default();
    let viewport = page_viewport(page).await.unwrap_or((1280, 800));
    let console_entries = drain_console(console);

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

async fn run_js(page: &Page, script: &str, label: &str) -> Response {
    match page.evaluate(script).await {
        Ok(_) => Response::ok(label.to_string()),
        Err(e) => Response::err(format!("{label} failed: {e}")),
    }
}
