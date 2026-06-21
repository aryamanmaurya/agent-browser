# agent-browser

A persistent headless browser session for AI agents — a Rust equivalent of the
"Agent Browser" tool, built on `chromiumoxide` (Chrome DevTools Protocol).

The CLI is a thin client that talks to a long-lived **daemon** over a Unix
domain socket. On first use the daemon is auto-spawned (detached) and kept
alive across invocations, so each command costs ~5 ms instead of ~1–2 s of
Chromium startup. The daemon shuts itself down after an idle timeout or on an
explicit `close`.

## Why this exists

This is the "eyes during construction" tool — meant to sit alongside a
contract-based validator (like a Playwright validator). It does **not** assert;
it **shows**. An agent calls `snapshot` to look at what it just built, before
declaring done.

- `agent-browser` = the agent's eyes during construction (this tool)
- `playwright-validator` = the agent's judge at the gate (your existing tool)

You want both. Neither is sufficient alone.

## Build

```bash
# Requires a Rust toolchain (1.85+) and a Chromium binary on the system.
cargo build --release
# Binary: target/release/agent-browser
```

A Chromium binary is required at runtime. Set it via `--chrome` or the
`AGENT_BROWSER_CHROME` environment variable:

```bash
export AGENT_BROWSER_CHROME=/path/to/chrome
```

## Usage

Every command is a separate process invocation that connects to the daemon.
The first command auto-starts the daemon; subsequent commands reuse it.

```bash
agent-browser navigate http://localhost:3000/
agent-browser snapshot                  # screenshot (base64 PNG) + text + console
agent-browser snapshot --text-only      # text + console only (for text-only models)
agent-browser status                    # current URL, viewport, uptime
agent-browser click "text=Submit"       # selector: text=, role=, xpath=, css=, or bare CSS
agent-browser type "css=#name" "hello"  # focus + type (React-aware)
agent-browser select "css=#color" "red"
agent-browser press Enter
agent-browser scroll down
agent-browser hover "css=.menu-item"
agent-browser resize 1280 800
agent-browser viewport mobile           # mobile | tablet | desktop
agent-browser wait-for --kind text "Welcome" --timeout-ms 5000
agent-browser console                   # drain console logs since last snapshot
agent-browser network                   # drain all network requests (method, URL, status, MIME, size)
agent-browser cookies
agent-browser local-storage             # all keys, or: local-storage mykey
agent-browser back
agent-browser forward
agent-browser reload
agent-browser close                     # stop the daemon
```

### Selector syntax

| Syntax | Meaning |
|---|---|
| `css=button.primary` | Raw CSS selector (native lookup) |
| `text=Submit` | Element whose trimmed `textContent` equals "Submit" (most specific match) |
| `role=button[name=Save]` | ARIA role + accessible name |
| `xpath=//button[@id="x"]` | XPath expression |
| `button.primary` (bare) | Treated as CSS |

### Snapshot output format

Designed for LLM consumption — structured sections, parseable markers:

```
=== AGENT-BROWSER SNAPSHOT ===
URL: http://localhost:3000/
Title: My App
Viewport: 1280 x 800
=== CONSOLE (since last snapshot) ===
[error] Hydration mismatch in <div> (console)
=== VISIBLE TEXT ===
Welcome
Click me
=== SCREENSHOT (base64 PNG, 45230 bytes) ===
iVBORw0KGgo...
=== END SNAPSHOT ===
```

The screenshot is a base64 PNG printed inline. If the orchestrator's model is
multimodal, pass the base64 (as a data URL) to the model so it can *see* the
page. If the model is text-only, use `snapshot --text-only` to skip the image.

The snapshot also includes a `=== NETWORK FAILURES ===` section showing any
requests that failed since the last snapshot (network errors, 4xx/5xx that
the browser couldn't reach, etc.). Use the `network` command for the full
request log including successes.

## Network capture

The daemon captures all network requests via CDP's Network domain. Each entry
includes: method, URL, resource type (Document/XHR/Fetch/Image/etc.), HTTP
status, MIME type, response size, and failure info.

- **`snapshot`** surfaces failed requests only (signal, not noise).
- **`network`** drains and returns ALL requests (success + failure).
- The buffer is capped at 1000 entries (oldest dropped on overflow).

Example `network` output:
```
=== NETWORK (7 requests) ===
GET http://localhost:3000/ → 200 (text/html 1.2KB) [Document]
GET http://localhost:3000/_next/static/chunk.js → 200 (application/javascript 45KB) [Script]
POST http://localhost:3000/api/data → 500 (application/json 0.3KB) [Fetch]
GET http://localhost:3000/missing.png → FAILED (net::ERR_FILE_NOT_FOUND) [Image]
```

## MCP server mode

The `mcp` subcommand runs agent-browser as an MCP (Model Context Protocol)
server over stdio. An MCP client (Claude Desktop, Cursor, any MCP-compatible
client) spawns this process and exchanges newline-delimited JSON-RPC 2.0
messages. All 20 browser commands are exposed as MCP tools.

```bash
agent-browser mcp
```

**Key advantage:** the `snapshot` tool returns TWO content blocks — a text
block (URL, title, console, network failures, visible text) AND an image
block (base64 PNG). Multimodal models like Claude 3.5 Sonnet / GPT-4o can
actually *see* the page screenshot, not just read its text.

### Claude Desktop configuration

Add to `claude_desktop_config.json`:
```json
{
  "mcpServers": {
    "agent-browser": {
      "command": "/path/to/agent-browser",
      "args": ["mcp"],
      "env": {
        "AGENT_BROWSER_CHROME": "/path/to/chrome"
      }
    }
  }
}
```

### Available MCP tools (20)

`navigate`, `snapshot`, `status`, `click`, `type`, `press`, `scroll`, `hover`,
`select`, `resize`, `viewport`, `wait_for`, `console`, `cookies`,
`local_storage`, `network`, `back`, `forward`, `reload`, `close`

Each tool's `inputSchema` is a JSON Schema object. The `snapshot` tool accepts
an optional `text_only` boolean to skip the screenshot (for text-only models).

### Protocol details

- Transport: stdio, newline-delimited JSON-RPC 2.0
- Protocol version: 2024-11-05
- The server launches its own Chromium instance (separate from the daemon)
- All logging goes to stderr; stdout is reserved exclusively for MCP messages
- On stdin EOF (client disconnect), the server closes the browser and exits

## Architecture

```
┌──────────────────────────────────────────────┐
│  CLI (agent-browser <command>)               │  ← one process per command
│   • parses args, builds Request              │
│   • connects to daemon UDS (auto-spawns it)  │
│   • sends Request, reads Response, prints    │
├──────────────────────────────────────────────┤
│  Unix domain socket (~/.cache/agent-browser/sock) │
├──────────────────────────────────────────────┤
│  Daemon (long-lived, detached)               │
│   • holds chromiumoxide Browser + Page open  │
│   • pumps CDP handler on a background task   │
│   • captures console + network events        │
│   • idle timeout: 10 min                     │
│   • serial Request → Response per connection │
├──────────────────────────────────────────────┤
│  Chromium (headless, --no-sandbox)           │
└──────────────────────────────────────────────┘
```

**Wire protocol:** 4-byte big-endian length prefix + JSON `Request`/`Response`
(see `src/protocol.rs`). One request per connection.

## Opencode integration

To wire this into your opencode orchestrator, register it as a skill that the
coding agent loads. The skill's instructions should tell the agent to call
`snapshot` after every UI change, before handing off to the validator.

Example `agent-browser` skill guidance (paraphrase into your skill format):

> After making any UI change, before declaring the feature done:
> 1. `agent-browser navigate <dev-url>`
> 2. `agent-browser snapshot` (or `--text-only` if your model can't see images)
> 3. Review the visible text + console + screenshot for obvious problems
>    (blank page, hydration errors, failed fetches, broken layout).
> 4. If anything looks wrong, fix it and re-snapshot.
> 5. Only then hand off to the validator for contract verification.

Keep `playwright-validator` as your TDD gate. `agent-browser` is the earlier,
lighter "does this look right" loop that runs during construction.

## Limitations / known notes

- **Chromium version sensitivity:** chromiumoxide 0.7 may log
  `"data did not match any variant of untagged enum Message"` for very new
  Chrome versions. These are non-fatal (unrecognized CDP events are skipped),
  but if a specific command stops working, try an older Chromium build.
- **`press` for named keys** dispatches CDP `Input.dispatchKeyEvent` with
  `key`+`code` set to the same value. Covers Enter/Escape/Tab; complex key
  chords (Shift+letter, Ctrl+Key) are not yet implemented.
- **One page per daemon/MCP server.** Tab/popup handling is not implemented.
- **`cargo build` produces a ~140 MB debug binary.** Use `--release` for a
  smaller, faster binary (~13 MB).
- **MCP mode runs its own browser** (separate from the CLI daemon). If you
  run both simultaneously, they use separate Chromium processes.

## Files

```
src/
  main.rs        CLI parsing (clap), request building, daemon/MCP dispatch
  protocol.rs    Request/Response enums + length-prefixed frame helpers
  daemon.rs      Persistent daemon: browser lifecycle, UDS server, console + network capture
  mcp.rs         MCP server over stdio: JSON-RPC, tool schemas, multimodal snapshot
  client.rs      CLI client: connect/auto-spawn, send, print (LLM-friendly format)
  commands.rs    Command implementations against the chromiumoxide Page
  selector.rs    text=/role=/xpath=/css= selector resolution
```

## License

MIT
