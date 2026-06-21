//! Selector resolution.
//!
//! Supported selector syntaxes (case-sensitive prefix):
//! - `css=...`           — raw CSS selector (passed to chromiumoxide native lookup)
//! - `text=Foo`          — element whose trimmed textContent equals "Foo" (most specific match)
//! - `role=button[name=Save]` — ARIA role + accessible name
//! - `xpath=//button`    — XPath expression
//! - bare string         — treated as CSS (most predictable default for an agent)
//!
//! For non-CSS selectors we return a JS expression that evaluates to the
//! element (or null). Callers then act on the element from JS.

/// A resolved selector: either a CSS string for native lookup, or a JS
/// expression that returns a single DOM element (or null).
pub enum Resolved {
    Css(String),
    /// A JS expression (without surrounding parens) that evaluates to an
    /// element or null. Wrap as `(<expr>)` when interpolating.
    Js(String),
}

pub fn resolve(selector: &str) -> Resolved {
    if let Some(rest) = selector.strip_prefix("css=") {
        return Resolved::Css(rest.to_string());
    }
    if let Some(rest) = selector.strip_prefix("text=") {
        let escaped = js_quote(rest);
        return Resolved::Js(format!(
            "(function(text) {{\
               const all = document.querySelectorAll('*');\
               let best = null;\
               for (const el of all) {{\
                 const tag = el.tagName;\
                 if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT') continue;\
                 const t = (el.textContent || '').trim();\
                 if (t === text) {{\
                   if (!best || (el.textContent||'').length < (best.textContent||'').length) best = el;\
                 }}\
               }}\
               return best;\
             }})({})",
            escaped
        ));
    }
    if let Some(rest) = selector.strip_prefix("role=") {
        // Format: role=button[name=Save]  (name is optional; other attrs ignored for now)
        let (role, name) = match rest.split_once('[') {
            Some((r, attrs)) => {
                let attrs = attrs.trim_end_matches(']');
                let name = attrs
                    .split(',')
                    .find_map(|kv| kv.trim().strip_prefix("name=").map(|s| s.to_string()));
                (r.to_string(), name)
            }
            None => (rest.to_string(), None),
        };
        let role_escaped = js_quote(&role);
        let name_js = match &name {
            Some(n) => js_quote(n),
            None => "null".to_string(),
        };
        return Resolved::Js(format!(
            "(function(role, name) {{\
               const sel = `[role=\"${{role}}\"], ${{role}}`;\
               const els = document.querySelectorAll(sel);\
               if (!name) return els[0] || null;\
               for (const el of els) {{\
                 const acc = el.getAttribute('aria-label')\
                   || el.getAttribute('alt')\
                   || el.getAttribute('title')\
                   || (el.textContent || '').trim();\
                 if (acc === name) return el;\
               }}\
               return null;\
             }})({}, {})",
            role_escaped, name_js
        ));
    }
    if let Some(rest) = selector.strip_prefix("xpath=") {
        let escaped = js_quote(rest);
        return Resolved::Js(format!(
            "(function(xpath) {{\
               const r = document.evaluate(xpath, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);\
               return r.singleNodeValue;\
             }})({})",
            escaped
        ));
    }
    // Bare selector → CSS.
    Resolved::Css(selector.to_string())
}

/// Escape a string as a JS string literal (single-quoted), public for reuse.
pub fn js_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}
