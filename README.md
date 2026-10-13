# Browser MCP

[![npm](https://img.shields.io/npm/v/@mirsella/opencode-browser-mcp)](https://www.npmjs.com/package/@mirsella/opencode-browser-mcp)
[![CI](https://github.com/mirsella/browser-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mirsella/browser-mcp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

One shared browser profile with separate tab ownership for OpenCode sessions
and Codex conversations. The plugin wraps
[`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp)
and routes each conversation through its own browser tabs while sharing cookies,
logins, local storage, cache, and extensions.

## Requirements

- OpenCode 1.18 or newer, or Codex with plugin and hook support
- Linux x64
- Bun
- A local Chromium-family browser
- A graphical session, unless `OPENCODE_BROWSER_HEADLESS=true`

## OpenCode setup

Add the plugin to `opencode.json`. The browser server is disabled by default,
so the entry shows up in `opencode mcp list` as disabled and the skill is
skipped. Opt in with plugin options:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": [["@mirsella/opencode-browser-mcp", { "enabled": true }]]
}
```

Restart OpenCode after changing its configuration. That is the complete setup.
You do not need a separate installation, broker command, or background service.

The first adapter starts the packaged native broker automatically.
Concurrent Codex and OpenCode instances converge on the same private socket and browser
profile. The broker remains available while at least one adapter is connected,
then closes the browser and exits shortly after the final adapter disconnects.

## Codex setup

The release tarball contains the executable, both integrations, and a local
Codex marketplace. Download it from [GitHub Releases](https://github.com/mirsella/browser-mcp/releases),
extract it to a permanent directory, then register that directory:

```sh
codex plugin marketplace add /path/to/browser-mcp
codex plugin add browser@browser-mcp
```

The npm package keeps its current name, `@mirsella/opencode-browser-mcp`.
To use a GitHub release in OpenCode, set the plugin entry to the `file:///absolute/path`
of its unpacked directory instead of the npm name.

For a source checkout, build and stage the binary first (`bun run build:native`
and `bun run stage:native`). Use a packaged directory for installation so
compiler outputs are not copied into Codex's plugin cache.

The installed plugin is enabled by default. Open `/hooks` in Codex to review
and trust its two hooks, then restart Codex. They inject the conversation ID
before browser calls and release owned tabs at `SessionEnd`. Browser calls
without an injected identity fail closed. The plugin exposes compact `help`
and `dispatch` tools and loads the same browser automation skill as OpenCode.

Codex and OpenCode use separate ownership namespaces while sharing the existing
broker, profile, cookies and logins. Codex's hook API identifies subagents with
their parent conversation ID, so subagents share that conversation's tabs.
Use separate pages when they work concurrently.

Codex runs `SessionEnd` on normal shutdown, on archive/delete of an open
conversation, or after 30 minutes idle with no client connected. Its hook has
a three-second deadline: the broker acknowledges cleanup promptly, then waits
for active calls before closing only that conversation's owned tabs. Cleanup
does not start a broker when none is running. Crashes still use normal broker
shutdown and idle cleanup.

The Codex MCP entry uses a 540-second tool timeout. Add `--direct-tools` to its
arguments when configuring per-action Codex tool approval rules; compact
dispatch has one approval rule for all actions.

## Plugin behavior

The plugin:

- registers the packaged browser MCP adapter (disabled unless opted in);
- injects the active OpenCode session ID after model argument generation;
- exposes compact browser tools unless existing permissions require direct tools;
- releases owned tabs when an OpenCode session is deleted;
- registers the bundled browser automation skill when the server is enabled.

OpenCode sees two compact tools by default:

- `browser_dispatch` executes one browser action.
- `browser_help` lists actions or loads one action schema.

If the plugin detects per-action `browser_*` permission or tool rules, it uses
direct tools so those policies continue to work.

Settings:

- `enabled` (default `false`): start the server and load the skill. An
  explicit `mcp.browser` entry with `"enabled": true` also opts in, without
  plugin options.
- `compactTools` (default `true`): keep `browser_dispatch` and `browser_help`
  instead of the direct tools. Set it to `false` to always use direct tools:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": [
    ["@mirsella/opencode-browser-mcp", { "enabled": true, "compactTools": false }]
  ]
}
```

## Architecture

```mermaid
flowchart LR
    O1[OpenCode instance A] --> A1[packaged stdio adapter]
    O2[OpenCode instance B] --> A2[packaged stdio adapter]
    A1 -->|private Unix socket| R[auto-managed broker]
    A2 -->|private Unix socket| R
    R --> T[CDP target tracker]
    R --> W[session worker pool]
    W --> C[chrome-devtools-mcp]
    T --> B[managed browser]
    C --> B
    B --> P[shared locked profile]
```

The adapter starts the broker on demand but does not own it. This lets adapters
from separate OpenCode processes share one browser without tying its lifetime to
whichever process connected first. Socket and profile locks make simultaneous
startup race-safe and prevent two browsers from opening the same profile.

Each routed session leases its own `chrome-devtools-mcp` worker. Stable
browser-level CDP target IDs remain the ownership authority; worker-local page
numbers are removed from model-visible schemas and responses.

## Ownership guarantees

- A tab has at most one owner.
- Routed sessions execute concurrently through separate workers.
- Popups inherit the owner of their opener tab.
- A session cannot select, inspect, or close another session's tabs through the broker.
- Manual and otherwise unowned tabs are not claimed during cleanup.
- Ownership conflicts, stale browser generations, and malformed responses fail closed.

These are routing guarantees, not an operating-system security boundary. Any
process running as the same Unix user can connect directly to the browser's
loopback CDP port.

## Browser and profile

The plugin detects Chromium, Google Chrome, Helium, Brave, and Microsoft Edge.
Chromium is preferred by default; `OPENCODE_BROWSER_EXECUTABLE` overrides
automatic detection.
Google Chrome 136 and newer ignore remote-debugging flags for its default data
directory, so Chrome uses a dedicated directory under
`~/.config/opencode-browser-mcp` by default. Other detected browsers retain their
existing profile defaults.

The broker starts Chromium lazily on the first browser action. For a headed
browser it passes only a fixed allowlist of display and desktop variables. The
current process environment stays authoritative; missing values are recovered
from the current user session when available.

Configuration variables inherited from either host (the existing names are
shared by both integrations):

| Variable | Default | Purpose |
| --- | --- | --- |
| `OPENCODE_BROWSER_EXECUTABLE` | auto-detected | Chromium-family executable |
| `OPENCODE_BROWSER_PROFILE` | browser-dependent | Shared browser data directory |
| `OPENCODE_BROWSER_HEADLESS` | `false` | Run without a visible window |
| `OPENCODE_BROWSER_SESSION_LAYOUT` | `shared-window` | `shared-window` or `window-per-session` |
| `OPENCODE_BROWSER_CDP_PORT` | `9222` | Loopback CDP port |
| `OPENCODE_BROWSER_IDLE_MINUTES` | `30` | Fallback cleanup for routed sessions |
| `OPENCODE_BROWSER_MAX_PAGES` | `5` | Max owned tabs per agent session |
| `OPENCODE_BROWSER_STARTUP_SECONDS` | `20` | Browser startup deadline |
| `OPENCODE_BROWSER_OPERATION_SECONDS` | `540` | Browser operation deadline |
| `OPENCODE_BROWSER_DOWNSTREAM_COMMAND` | `bunx` | DevTools MCP launcher |

## Lifecycle

| Event | Behavior |
| --- | --- |
| First adapter connects | One internal broker acquires the socket and profile leases |
| Tools are listed | Chromium remains stopped |
| First browser action | Chromium and the ownership tracker start |
| OpenCode turn becomes idle | Owned tabs remain available for the next turn |
| OpenCode session is deleted | Only that session's owned tabs close |
| Each session owns at most 5 pages | `new_page` beyond the cap fails; reuse a page with `navigate_page` or free one with `close_page` |
| Routed session reaches the configured inactivity timeout | Fallback cleanup closes that session's owned tabs; defaults to 30 minutes and `0` disables it |
| Another OpenCode instance connects | It shares the broker and profile with isolated tab ownership |
| Final adapter disconnects | The broker waits briefly, closes Chromium, and exits |
| Browser exits, segfaults, or is killed | The next browser action starts Chromium again; ownership from the old browser generation is discarded |
| DevTools worker exits or loses its transport | The next call replaces the worker and restores routing to surviving owned tabs |
| Broker exits unexpectedly | Connected adapters stay available and start one replacement on the next call; surviving tabs are restored only when the exact browser generation matches |
| MCP adapter exits unexpectedly | The OpenCode plugin reconnects it automatically, with retries up to 30 seconds apart |

Calls interrupted after dispatch can have taken effect. They return an error and
are never replayed automatically. Inspect the browser state before retrying an
action. Explicitly disabled or disconnected MCP entries stay disabled.

## Troubleshooting

- A headed startup error mentioning `DISPLAY` or `WAYLAND_DISPLAY` means no graphical session could be found. Set `OPENCODE_BROWSER_HEADLESS=true` when running without one.
- A profile or socket lease error means another live package process owns it or a stale process is still running. Do not bypass the locks.
- A process on the configured CDP port must belong to the exact managed browser generation. Stop it or choose another port.
- If downstream startup fails, verify that `bunx` is available to OpenCode.
- The plugin refuses to replace an unrelated `mcp.browser` entry.

## Security

All sessions deliberately share cookies, authentication, storage, extensions,
permissions, history, and downloads. Browser content remains untrusted and may
contain prompt injection. The private same-UID socket prevents access by other
local users, but it does not isolate processes already running as your Unix user.

Read [`SECURITY.md`](SECURITY.md) before using a profile with sensitive data.

## Development

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
bun test tests/plugin.test.ts
cargo test --release --test e2e -- --ignored --nocapture --test-threads=1
```

The headed E2E starts adapters against temporary profiles, sockets, and CDP
ports. It checks automatic startup, startup races, cookie sharing, tab isolation,
concurrent calls, active-call cleanup, reconnect grace, compact
dispatch, both window layouts, and automatic browser shutdown.

| Path | Responsibility |
| --- | --- |
| `plugin/` | OpenCode registration, session routing, and cleanup controls |
| `vendor/` | packaged native binary used directly by the OpenCode plugin |
| `src/broker.rs` | adapter bridge, managed broker lifecycle, routing, and leases |
| `src/cdp.rs` | stable CDP target tracking and ownership |
| `src/main.rs` | browser supervision, workers, and response filtering |
| `src/persistence.rs` | atomic generation-scoped ownership state |
| `tests/e2e.rs` | startup, shared-profile, isolation, routing, and shutdown coverage |

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the development and release workflow.

## License

[MIT](LICENSE)
