# opencode-browser-mcp

[![CI](https://github.com/mirsella/opencode-browser-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mirsella/opencode-browser-mcp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Browser automation for [OpenCode](https://opencode.ai) using Chrome DevTools and a shared Chromium profile.

Each OpenCode session gets its own tabs. A local daemon coordinates ownership across separate OpenCode processes, so concurrent agents cannot select, inspect, or close each other's tabs. Cookies, logins, extensions, and other profile data remain shared.

## Requirements

- Linux x64
- OpenCode 1.18 or newer
- Bun
- Node.js 20 or newer
- A Chromium-based browser

Helium, Chromium, Google Chrome, Brave, and Microsoft Edge are auto-detected. Helium is preferred when installed. Set `OPENCODE_BROWSER_EXECUTABLE` and `OPENCODE_BROWSER_PROFILE` to use another executable or profile.

## Install

The npm package is `@mirsella/opencode-browser-mcp`. Its first release is pending. After publication, add it to `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["@mirsella/opencode-browser-mcp"]
}
```

Restart OpenCode after changing the configuration. The plugin installs the browser skill, registers the MCP server, and injects the trusted OpenCode session ID used for tab isolation.

Do not configure the package as a bare MCP command. The OpenCode plugin hook is required for session routing.

The package also provides a CLI:

```sh
npx -y @mirsella/opencode-browser-mcp --help
npx -y @mirsella/opencode-browser-mcp --stop-daemon
```

## How it works

```text
OpenCode A ─┐
            ├─ stdio bridge ─ Unix socket ─ browser daemon
OpenCode B ─┘                              ├─ CDP target tracker
                                          └─ one worker per session
```

Chrome's stable tab target ID is the ownership key. Numeric page IDs from `chrome-devtools-mcp` remain private to each worker. Popups inherit ownership through Chrome's opener lineage.

The daemon starts the browser on the first connection. Two seconds after the last OpenCode client disconnects, it stops the workers, closes the browser, and clears ownership state. Per-session idle cleanup closes only that session's tabs while other clients remain connected.

Ownership state is stored atomically beside the daemon socket. If the daemon crashes but the browser survives, ownership is restored only when the browser generation and live tab targets still match.

## Configuration

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `OPENCODE_BROWSER_EXECUTABLE` | auto-detected | Chromium-based browser executable |
| `OPENCODE_BROWSER_PROFILE` | detected browser profile | Persistent browser profile |
| `OPENCODE_BROWSER_SOCKET` | `$XDG_RUNTIME_DIR/opencode-browser-mcp/daemon.sock` | Daemon socket |
| `OPENCODE_BROWSER_CDP_HOST` | `127.0.0.1` | CDP host |
| `OPENCODE_BROWSER_CDP_PORT` | `9222` | CDP port |
| `OPENCODE_BROWSER_IDLE_MINUTES` | `60` | Session idle cleanup; `0` disables expiry |
| `OPENCODE_BROWSER_STARTUP_SECONDS` | `20` | Browser startup deadline |
| `OPENCODE_BROWSER_OPERATION_SECONDS` | `540` | Browser operation deadline |
| `OPENCODE_BROWSER_DOWNSTREAM_COMMAND` | `bunx` | `chrome-devtools-mcp` launcher |
| `OPENCODE_BROWSER_DISABLE_CRASH_REPORTER` | `false` | Disable Chromium crash reporting |

The first daemon fixes these values for its lifetime. Clients with different settings are rejected.

## Shared profile

Tab ownership is isolated, browser data is not. All sessions share the configured profile, including cookies, authentication, local storage, extensions, cache, permissions, and downloads. This is intentional so browser agents can use existing signed-in sessions.

## Development

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

The npm artifact requires Rust 1.89 or newer and the musl target:

```sh
rustup target add x86_64-unknown-linux-musl
npm pack
```

Browser tools are provided by [`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp), pinned to version 1.6.0.

## License

[MIT](LICENSE)
