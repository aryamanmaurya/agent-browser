# agent-browser

A persistent browser for AI agents — launch headful/headless, capture network
requests, record real user input, and inspect pages. Built on
`chromiumoxide` (Chrome DevTools Protocol) in Rust.

## What it is

**"An eye for coding agents"**. it**Allows** an agent see UI, 
opens it in a visible (headful) window for a human to test by hand
while capturing real time network calls `snapshot` / `console` / `network` /
`interactions` to see what actually happened — including interactions with no
console/network side effect.


## How it works

A thin CLI talks to a long-lived **daemon** over a Unix domain socket. The
first command auto-spawns the daemon (detached); subsequent commands reuse it,
so each call costs ~5 ms instead of ~1–2 s of Chromium startup. The daemon
shuts down after 10 min idle or on explicit `close`.

## Build & configure

```bash
cargo build --release            # binary: target/release/agent-browser
export AGENT_BROWSER_CHROME=/path/to/chromium   # or pass --chrome
```

Requires a Rust toolchain (1.85+) and a Chromium binary at runtime.

## Sessions (multi-agent concurrency)

Each `--session <name>` is an independent daemon + browser + socket, so agents
run concurrently without interfering. Each session has its own URL, buffers,
and headless/headful mode. Names: `[a-zA-Z0-9_-]`, max 64 chars; ~100–300 MB
RAM each; auto-shutdown after 10 min idle.

```bash
agent-browser --session agent1 navigate http://site-a.com
agent-browser --session agent2 --headful snapshot      # visible window
agent-browser --session agent2 status
agent-browser sessions                                  # list all sessions
agent-browser --session agent1 stop
agent-browser --session all stop                        # stop everything
```

- `--session <name>` picks the daemon (default: `default`; env: `AGENT_BROWSER_SESSION`)
- `--headful` / `--visible` launches a visible window (newly-spawned daemons only)

## Commands to use

| Command | Description |
|---|---|
| `daemon` | Run daemon in foreground (debugging) |
| `status` | Current URL, viewport, uptime |
| `stop` / `close` | Stop / close session |
| `sessions` | List active sessions |
| `navigate <url> [--timeout-ms]` | Go to a URL |
| `back` / `forward` / `reload` | Browser history |
| `snapshot [--text-only]` | Screenshot + text + console + network failures |
| `console` | Drain console logs |
| `network [--filter /api/]` | Drain all requests (with bodies for small JSON) |
| `interactions [--filter type]` | Drain real user input events |
| `click` / `type` / `press` / `select` / `hover` | Drive the page |
| `scroll` | Scroll the page |
| `resize <w> <h>` / `viewport <preset>` | Viewport (mobile/tablet/desktop) |
| `wait-for --kind selector\|text\|url <target>` | Wait for a condition |
| `cookies` / `local-storage [key]` | Page state |
| `throttle <preset>` | Network conditions |
| `pdf <path>` | Print page to PDF |
| `inspect <selector>` | Tag, attrs, styles, bbox, ARIA |
| `accessibility` | ARIA tree as indented text |
| `har <path>` | Export network as HAR 1.2 (read-only) |
| `tab-list` / `tab-new [url]` / `tab-switch <id>` / `tab-close <id>` | Multi-tab |
| `mcp [--headful]` | Run as an MCP server over stdio |

### Selector syntax

| Syntax | Meaning |
|---|---|
| `css=button.primary` | Raw CSS selector (native lookup) |
| `text=Submit` | Element whose trimmed `textContent` equals "Submit" |
| `role=button[name=Save]` | ARIA role + accessible name |
| `xpath=//button[@id="x"]` | XPath expression |
| `button.primary` (bare) | Treated as CSS |

## Observation

**Snapshot** — output is structured for LLM consumption with parseable
markers: URL, title, viewport, console (since last snapshot), network
failures (signal, not noise), visible text, and a base64 PNG screenshot
(skipped with `--text-only` for text-only models).

**Console** — drains since the last drain. Marker entries from the
interaction recorder are filtered out, so `console` stays clean.

**Network** — captures method, URL, resource type, status, MIME, size, failure
info, and response bodies for small JSON/text (<64 KB). Buffer capped at 1000.
`snapshot` surfaces only failed requests (peeked, not drained); `network`
drains all; `har <path>` exports as HAR 1.2 **without** draining.

## Interaction recording

The daemon injects a small JS recorder into every document (initial page, new
tabs, iframes, MPA navigations). It captures **real DOM input events** —
whether performed by the human in a headful window or the agent via CLI. This
makes *silent* interactions visible (toggles with no side effect, local state,
unsubmitted typing).

```bash
agent-browser interactions                    # drain all events
agent-browser interactions --filter click     # keep only clicks
```

- Captures `click`, `keydown` (per key, with key name), `input`/`change`
  (values), `submit` (forms).
- Each entry: event type, timestamp, target selector, coordinates, value, and
  the route (`path` = `location.pathname`) read at event time — SPA-safe.
- Buffer shared across tabs, capped at 2000, drains on read.
- Caveats: no per-tab/frame id, best-effort Shadow DOM selectors, human vs
  agent sources not distinguished in v1.

## Element inspection & accessibility

`inspect <selector>` returns tag, attributes, text, bounding box, computed
styles (14 common properties), and ARIA role/label. `accessibility` walks the
DOM and computes ARIA roles (implicit mappings + explicit `role` attributes)
as indented text.

## Network throttling

```bash
agent-browser throttle slow-3g   # ~400ms latency, 50KB/s down
agent-browser throttle fast-3g   # ~150ms latency, 250KB/s down
agent-browser throttle offline   # simulate disconnection
agent-browser throttle none      # disable
```

## Multi-tab

The daemon tracks multiple tabs; console/network/interaction buffers are
shared across tabs. New tabs inherit the default viewport and get their own
listeners. Closing the active tab switches to the first remaining tab.

## MCP server mode

Exposes all commands as MCP tools over stdio (newline-delimited JSON-RPC 2.0)
for Claude Desktop, Cursor, etc. 26 tools: `navigate`, `snapshot`, `status`,
`click`, `type`, `press`, `scroll`, `hover`, `select`, `resize`, `viewport`,
`wait_for`, `console`, `cookies`, `local_storage`, `network`,
`interactions`, `back`, `forward`, `reload`, `close`, `throttle`, `pdf`,
`inspect`, `accessibility`, `har`.

The `snapshot` tool returns **two** content blocks — text (URL, title,
console, network failures, visible text) **and** an image (base64 PNG), so
multimodal models can actually see the page.

```bash
agent-browser mcp                 # headless
agent-browser mcp --headful       # visible window
```

Claude Desktop config:
```json
{
  "mcpServers": {
    "agent-browser": {
      "command": "/path/to/agent-browser",
      "args": ["mcp", "--headful"],
      "env": { "AGENT_BROWSER_CHROME": "/path/to/chrome" }
    }
  }
}
```

Protocol notes: stdio, JSON-RPC 2.0, protocol version 2024-11-05. The server
runs its own Chromium (separate from any CLI daemon). Logging goes to stderr;
stdout is reserved for MCP messages. On stdin EOF it closes the browser and
exits.

## Architecture

```
┌───────────────────────────────────────────────┐
│  CLI (agent-browser <command>)  ← one process │
│   parses args → Request → connect (auto-spawn)│
├───────────────────────────────────────────────┤
│  Unix domain socket (~/.cache/agent-browser/) │
├───────────────────────────────────────────────┤
│  Daemon (long-lived, detached)                │
│   • chromiumoxide Browser + Page(s) open      │
│   • captures console / network / interactions │
│   • multi-tab, 10 min idle timeout            │
├───────────────────────────────────────────────┤
│  Chromium (headless, --no-sandbox)            │
└───────────────────────────────────────────────┘
```

Wire protocol: 4-byte big-endian length prefix + JSON `Request`/`Response`
(`src/protocol.rs`), one request per connection.

## Opencode integration

Register agent-browser as a skill the coding agent loads, telling it to call
`snapshot` after every UI change before handing off to the validator:

1. `agent-browser navigate <dev-url>`
2. `agent-browser snapshot` (or `--text-only` for text-only models)
3. Review visible text + console + screenshot for problems (blank page,
   hydration errors, failed fetches, broken layout).
4. Fix issues, re-snapshot.
5. Hand off to a contract-based validator for final verification.

## Limitations

- **Chromium version sensitivity:** chromiumoxide 0.7 may log `"data did not
  match any variant of untagged enum Message"` on very new Chrome builds
  (non-fatal). If a command stops working, try an older Chromium.
- **Response bodies** only for small JSON/text (<64 KB, truncated at 4 KB);
  binary/large payloads skipped.
- **HAR is read-only** (doesn't drain) — run `har` before `network` to export
  everything.
- **Multi-tab buffers are shared** (no per-tab isolation).
- **`press` for named keys** covers Enter/Escape/Tab; complex chords
  (Shift+letter, Ctrl+Key) not yet implemented.
- **MCP mode** uses its own Chromium process, separate from CLI daemons.
- **Accessibility tree** uses a JS DOM walker (more robust than CDP's
  `Accessibility.getFullAXTree` on some version combos).
- Use `--release` to avoid the ~140 MB debug binary.

## Files

```
src/
  main.rs        CLI parsing (clap), request building, daemon/MCP dispatch
  protocol.rs    Request/Response enums + entry types + frame helpers
  daemon.rs      Daemon: browser lifecycle, UDS server, multi-tab, capture
  mcp.rs         MCP server over stdio: JSON-RPC, tool schemas, multimodal snapshot
  client.rs      CLI client: connect/auto-spawn, send, print (LLM-friendly)
  commands.rs    Command implementations (browse, inspect, capture, etc.)
  selector.rs    text=/role=/xpath=/css= selector resolution
```

## License

MIT

## Personal notes

umm, a tool like agent browser gives your coding agents the ability to test and validate their own output & less manual work.
