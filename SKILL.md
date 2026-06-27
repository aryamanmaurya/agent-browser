---
name: agent-browser
description: >
  Use agent-browser for persistent headless browser sessions during
  construction. Call `snapshot` after every UI change to look at what you
  just built — it does NOT assert, it SHOWS. A daemon-based tool with ~5ms
  per command latency, network capture, screenshots, console logs,
  accessibility inspection, multi-tab support, and MCP server mode.
---

# agent-browser

Persistent headless browser session for AI agents (chromiumoxide-backed).

A persistent headless browser session for AI agents. The CLI talks to a
long-lived daemon over a Unix domain socket. The first command auto-spawns the
daemon; subsequent commands reuse it (~5ms per command, no cold-start latency).

## Prerequisites

- Chromium binary, set via `AGENT_BROWSER_CHROME` env var
- Use `chromium-1228` version (recommended)

## Binary

```
~/opencode/agent-browser/target/release/agent-browser   (source build)
~/.agents/skills/agent-browser/agent-browser            (standalone copy)
~/.local/bin/agent-browser                               (symlink)
```

### Set up

```bash
export AGENT_BROWSER_CHROME="$HOME/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome"
export PATH="$HOME/.local/bin:$PATH"
```

### Rebuild (after pulling new code)

```bash
cd ~/opencode/agent-browser
cargo build --release
cp target/release/agent-browser ~/.agents/skills/agent-browser/agent-browser
ln -sf ~/.agents/skills/agent-browser/agent-browser ~/.local/bin/agent-browser
```

## Options

| Option | Description |
|--------|-------------|
| `--chrome <PATH>` | Path to Chromium binary (overrides `$AGENT_BROWSER_CHROME`) |
| `--socket <PATH>` | Path to daemon Unix socket (overrides `--session`) |
| `--session <NAME>` | Session name for independent daemon + browser. Default: `"default"`. Env: `$AGENT_BROWSER_SESSION` |
| `--headful` | Launch browser with visible window (only affects newly-spawned daemons) |

## Workflow

After making any UI change during development:

1. `agent-browser navigate <dev-url>`
2. `agent-browser snapshot` (or `--text-only` for text-only models)
3. Review visible text + console + screenshot for problems
4. Fix issues, re-snapshot
5. Done

## Commands

### Daemon & Session Management
| Command | Description |
|---------|-------------|
| `daemon` | Run daemon in foreground (auto-spawned; useful for debugging) |
| `status` | Show URL, viewport, uptime |
| `stop` | Stop the running daemon |
| `close` | Close session and stop daemon |
| `sessions` | List all active sessions (name, mode, URL, uptime) |

### Navigation
| Command | Description |
|---------|-------------|
| `navigate <url> [--timeout-ms 10000]` | Go to URL with optional timeout |
| `back` / `forward` / `reload` | Browser history |

### Observation
| Command | Description |
|---------|-------------|
| `snapshot` | Screenshot + text + console + network failures |
| `snapshot --text-only` | Skip screenshot |
| `console` | Drain console logs |
| `network [--filter /api/]` | Drain network requests (with response bodies) |
| `inspect <selector>` | Tag, attributes, computed styles, bbox, ARIA |
| `accessibility` | ARIA tree as indented text |

### Interaction
| Command | Description |
|---------|-------------|
| `click <selector>` | Click element (selector: `css=`, `text=`, `role=`, `xpath=`, or bare CSS) |
| `type <selector> <text>` | Focus + type (React-aware) |
| `press <key>` | Keyboard key (Enter, Escape, Tab, a, etc.) |
| `scroll` | Scroll the page |
| `hover <selector>` | Hover element (dispatches mouseover/mouseenter/mousemove) |
| `select <selector> <value>` | Select `<select>` option |

### Viewport
| Command | Description |
|---------|-------------|
| `resize <w> <h>` | Exact dimensions |
| `viewport mobile` | 375×667 |
| `viewport tablet` | 768×1024 |
| `viewport desktop` | 1280×800 |

### Network Conditions
| Command | Description |
|---------|-------------|
| `throttle offline` | Disconnection |
| `throttle slow-3g` | ~400ms latency, 50KB/s down |
| `throttle fast-3g` | ~150ms, 250KB/s down |
| `throttle none` | Disable |

### State
| Command | Description |
|---------|-------------|
| `cookies` | All cookies |
| `local-storage [key]` | localStorage entries |
| `har <path>` | Export HAR 1.2 JSON (read-only) |
| `pdf <path>` | Print to PDF |

### Multi-Tab
| Command | Description |
|---------|-------------|
| `tab-list` | List tabs |
| `tab-new [url]` | Open new tab |
| `tab-switch <id>` | Switch tab |
| `tab-close <id>` | Close tab |

### Waiting
| Command | Description |
|---------|-------------|
| `wait-for --kind selector <css>` | Wait for selector |
| `wait-for --kind text "Hello"` | Wait for text |
| `wait-for --kind url /path` | Wait for URL match |

### Other
| Command | Description |
|---------|-------------|
| `help` | Print help message |

## Selector Syntax

| Syntax | Example | Resolution |
|--------|---------|------------|
| `css=.my-class` | Raw CSS | Native CDP |
| `text=Submit` | Exact textContent | JS (picks shallowest match) |
| `role=button[name=Save]` | ARIA role + name | JS (aria-label, alt, title, text) |
| `xpath=//button` | XPath | JS document.evaluate |
| `button.primary` (bare) | Treated as CSS | Native CDP |

## Snapshot Output

```
=== AGENT-BROWSER SNAPSHOT ===
URL: http://localhost:3000/
Title: My App
Viewport: 1280 x 800
=== CONSOLE (since last snapshot) ===
[error] Hydration mismatch in <div>
=== NETWORK FAILURES ===
GET /missing.png → FAILED (net::ERR_FILE_NOT_FOUND)
=== VISIBLE TEXT ===
Welcome
Click me
=== SCREENSHOT (base64 PNG, 45230 bytes) ===
iVBORw0KGgo...
=== END SNAPSHOT ===
```

## Network Capture

- CDP Network domain captures all requests
- Response bodies for small JSON/text (<64KB, truncated at 4KB)
- 1000-entry buffer cap (oldest dropped)
- `snapshot` shows only **failed** requests (peeked, not drained)
- `network` drains ALL requests (success + failure)
- `har` exports as HAR 1.2 (read-only, doesn't drain)

## MCP Server Mode

For Claude Desktop, Cursor, etc.:

```bash
agent-browser mcp
```

Exposes all browser commands as MCP tools. `snapshot` returns text + image blocks (multimodal).

```json
{
  "mcpServers": {
    "agent-browser": {
      "command": "/home/aryaman/.local/bin/agent-browser",
      "args": ["mcp"],
      "env": {"AGENT_BROWSER_CHROME": "$HOME/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome"}
    }
  }
}
```

## Dos and Don'ts

- DO call `snapshot` after every UI change
- DO check console + network failures for silent errors
- DO use `inspect` for layout/style debugging
- DO use `throttle` for loading state testing
- DO use `accessibility` to verify ARIA roles
- DO use `--session <name>` for concurrent agents (e.g. frontend + backend)
- DON'T use for assertions — this shows, not asserts
- DON'T forget `AGENT_BROWSER_CHROME`
- DON'T forget to `stop` or `close` the daemon when done
