# opencode-browser-mcp

[![CI](https://github.com/mirsella/opencode-browser-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mirsella/opencode-browser-mcp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Browser automation for [OpenCode](https://opencode.ai) using Chrome DevTools and a real [Helium](https://helium.computer) profile.

The plugin gives each OpenCode session its own tabs while every session shares one headed browser, profile, and login state. A singleton daemon coordinates ownership across separate OpenCode processes. It hides internal page IDs and prevents one session from selecting, inspecting, or closing another session's tabs.

## Why this exists

Running `chrome-devtools-mcp` directly works well for one client, but multiple OpenCode sessions share its selected-page state. Concurrent calls can target the wrong tab, and separate MCP processes have no shared ownership registry.

`opencode-browser-mcp` adds that missing coordination layer:

- one Helium process and persistent profile
- one browser daemon shared by all local OpenCode processes
- one `chrome-devtools-mcp` worker per OpenCode session
- stable ownership based on Chrome tab target IDs and popup opener lineage
- concurrent calls across sessions, ordered calls within a session
- generation-bound ownership recovery when Helium survives a daemon crash
- automatic browser shutdown after the last OpenCode client disconnects

## Requirements

- Linux x64
- OpenCode 1.18 or newer
- Helium
- Bun, used to run the pinned `chrome-devtools-mcp@1.6.0` worker
- Node.js 20 or newer when installing through npm

The default executable search checks:

```text
/opt/helium-browser-bin/helium-wrapper
/usr/local/bin/helium
/usr/bin/helium
$PATH/helium
```

Override it with `OPENCODE_BROWSER_HELIUM_EXECUTABLE` if Helium is installed elsewhere.

## Install

### From npm

The scoped package is prepared as `@mirsella/opencode-browser-mcp`. The first npm release is pending. Once published, add it to the global or project `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["@mirsella/opencode-browser-mcp"]
}
```

OpenCode installs the package, loads the browser skill, registers the local MCP server, and injects the trusted OpenCode session ID into every `browser_*` call.

### From source

```sh
git clone https://github.com/mirsella/opencode-browser-mcp.git
cd opencode-browser-mcp
cargo build --release
```

Add the absolute plugin path to `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": [
    "/absolute/path/to/opencode-browser-mcp/plugin/browser-routing.ts"
  ]
}
```

Restart OpenCode after changing plugin configuration.

## npx usage

The npm package provides an `opencode-browser-mcp` executable, so these commands will work after the first npm release:

```sh
npx -y @mirsella/opencode-browser-mcp --help
npx -y @mirsella/opencode-browser-mcp --stop-daemon
```

Do not configure the bare `npx` command as an OpenCode MCP server. Session isolation depends on the plugin hook that injects OpenCode's trusted session ID. Loading the npm package through `plugin` config handles both pieces.

## How it works

```text
OpenCode process A ─┐
                   ├─ stdio bridge ─ Unix socket ─ browser daemon
OpenCode process B ─┘                              ├─ CDP target tracker
                                                  ├─ session A worker
                                                  ├─ session B worker
                                                  └─ shared Helium profile
```

The executable starts as a byte-for-byte stdio bridge. The first bridge starts a private per-user daemon; later bridges connect to the same Unix socket. The daemon owns the browser, profile lock, CDP connection, tab ownership registry, and downstream workers.

Chrome's stable `type: "tab"` target ID is the ownership key. Numeric page IDs from `chrome-devtools-mcp` stay private to each worker. Popups inherit ownership through Chrome's `openerId` target lineage.

## Browser lifecycle

The daemon stays alive while at least one OpenCode MCP client is connected. After the final client disconnects, it waits two seconds for a reconnect, then:

1. stops all downstream workers
2. sends `Browser.close` through CDP
3. confirms that the browser endpoint is gone
4. stops the target tracker
5. clears ownership state for that exact browser generation

Per-session idle cleanup is separate. It closes only the idle session's owned tabs while other clients continue using the browser.

Use the CLI to stop the daemon explicitly:

```sh
opencode-browser-mcp --stop-daemon
```

## Crash recovery

Ownership state is written atomically beside the daemon socket. The state contains stable tab IDs, session workspace bindings, current tabs, and activity times. It never stores worker-local numeric page IDs.

If the daemon dies but Helium survives, the next daemon restores ownership only when the browser WebSocket generation matches and the targets are still live. A different browser generation clears stale ownership instead of reusing old target IDs.

## Configuration

CLI options and environment variables use the same values. Run `opencode-browser-mcp --help` for the full CLI reference.

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `OPENCODE_BROWSER_HELIUM_EXECUTABLE` | auto-detected | Helium executable or wrapper |
| `OPENCODE_BROWSER_PROFILE` | `$XDG_CONFIG_HOME/net.imput.helium` or `~/.config/net.imput.helium` | Persistent browser profile |
| `OPENCODE_BROWSER_SOCKET` | `$XDG_RUNTIME_DIR/opencode-browser-mcp/daemon.sock` | Daemon Unix socket |
| `OPENCODE_BROWSER_CDP_HOST` | `127.0.0.1` | Local CDP bind host |
| `OPENCODE_BROWSER_CDP_PORT` | `9222` | CDP port |
| `OPENCODE_BROWSER_IDLE_MINUTES` | `60` | Per-session idle cleanup; `0` disables idle expiry |
| `OPENCODE_BROWSER_STARTUP_SECONDS` | `20` | Browser startup deadline |
| `OPENCODE_BROWSER_OPERATION_SECONDS` | `540` | Downstream browser operation deadline |
| `OPENCODE_BROWSER_DOWNSTREAM_COMMAND` | `bunx` | Command used to start `chrome-devtools-mcp` |
| `OPENCODE_BROWSER_DISABLE_CRASH_REPORTER` | `false` | Disable Chromium crash reporting, useful for isolated tests |

The first daemon process fixes these values for its lifetime. Clients with a different configuration are rejected instead of silently attaching to the wrong browser.

## Isolation boundary

This project isolates tab ownership, not browser data.

All sessions intentionally share the configured Helium profile. They share cookies, authentication, local storage, extensions, permissions, cache, downloads, and other profile state. A session cannot access another session's owned tabs through the MCP tools, but it can navigate its own tab to a site where the shared profile is already signed in.

The daemon accepts same-user Unix socket peers only. Runtime files are private, and existing browser endpoints are adopted only when their WebSocket generation matches persisted state.

## Development

Building the npm artifact requires Rust 1.89 or newer and the musl target:

```sh
rustup target add x86_64-unknown-linux-musl
```

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The headed integration test uses a temporary profile and CDP port:

```sh
cargo test --release --test e2e -- --ignored --nocapture
```

Build the npm tarball locally with:

```sh
npm pack
```

`prepack` builds a statically linked musl binary and stages it under `vendor/`. The current npm package targets Linux x64 only.

## Upstream

The browser tools come from [`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp), pinned to version 1.6.0. This project adds OpenCode routing, lifecycle management, and multi-session ownership around it.

## License

[MIT](LICENSE)
