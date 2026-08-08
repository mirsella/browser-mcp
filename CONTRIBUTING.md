# Contributing

Contributions should preserve the ownership model rather than working around
it in an adapter or response filter.

## Requirements

- Rust 1.89 or newer
- Bun
- Node.js 20.19+, 22.12+, or 23+ for npm packaging
- A compatible headed Chromium-family browser for the E2E test
- A display server or Xvfb for headed browser tests
- `x86_64-unknown-linux-musl` for the published native binary

## Checks

Run the normal checks before opening a pull request:

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
bun test tests/plugin.test.ts
```

Run the real-browser test for changes to routing, ownership, browser startup,
cleanup, persistence, or the daemon protocol:

```sh
cargo test --release --test e2e -- --ignored --nocapture
```

The E2E test uses a temporary profile, socket, and CDP port. It verifies generic
connection cleanup, OpenCode session isolation, concurrent calls, unowned-tab
preservation, compact-tool dispatch and schema reduction, daemon reconnection,
lazy Chromium activation, per-session window placement, and final MCP-transport
shutdown. The regular integration suite also verifies compact initialization
without a browser or downstream worker.

Check the npm artifact when changing package files or the native launcher:

```sh
rustup target add x86_64-unknown-linux-musl
npm pack --dry-run
```

## Design rules

- Stable browser-level CDP tab target IDs are the ownership authority.
- Worker-local numeric page IDs are disposable routes and must not reach models.
- A tab can have only one browser session owner.
- Existing or unowned tabs must not be claimed by inference.
- Cleanup closes only exact targets owned by the retiring session.
- Browser generation changes invalidate ownership instead of guessing.
- Ownership conflicts and malformed downstream responses fail closed.
- Generic MCP routing and OpenCode routing remain separate connection modes.
- Generic MCP clients keep direct tools; compact OpenCode tools disclose one
  current action schema at a time without duplicating upstream definitions.
- Per-action OpenCode permission or tool rules force direct mode rather than
  silently broadening access through the compact dispatcher.
- Tool discovery, compact help, and no-op session release must not activate the
  browser runtime.
- Window layout is best-effort presentation. Stable target ownership remains
  authoritative, and layout repair must not move, claim, or close tabs.

## Releases

Releases are published from GitHub Actions through npm trusted publishing. Do
not add an npm token to the repository or workflow.

1. Update npm package `@mirsella/opencode-browser-mcp` and Cargo package
   `opencode-browser-mcp` to the same version in `package.json`, `Cargo.toml`,
   and `Cargo.lock`.
2. Run the Rust checks, `bun test tests/plugin.test.ts`, headed E2E test, and
   `npm pack --dry-run`.
3. Push the release commit.
4. Create a GitHub release tagged `v<version>`.
5. The `publish` workflow builds the static binary and publishes with OIDC.
