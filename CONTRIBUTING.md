# Contributing

Contributions should preserve the ownership model rather than working around
it in an adapter or response filter.

## Requirements

- Rust 1.89 or newer
- Bun
- Node.js 20.19+, 22.12+, or 23+ for npm packaging
- A compatible headed Chromium-family browser for the E2E test
- `x86_64-unknown-linux-musl` for the published native binary

## Checks

Run the normal checks before opening a pull request:

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Run the real-browser test for changes to routing, ownership, browser startup,
cleanup, persistence, or the daemon protocol:

```sh
cargo test --release --test e2e -- --ignored --nocapture
```

The E2E test uses a temporary profile, socket, and CDP port. It verifies generic
connection cleanup, OpenCode session isolation, concurrent calls, unowned-tab
preservation, daemon reconnection, and final shutdown.

Check the npm artifact when changing package files or the native launcher:

```sh
rustup target add x86_64-unknown-linux-musl
npm pack --dry-run
```

## Design rules

- Stable browser-level CDP tab target IDs are the ownership authority.
- Worker-local numeric page IDs are disposable routes and must not reach models.
- A tab can have only one browser-session owner.
- Existing or unowned tabs must not be claimed by inference.
- Cleanup closes only exact targets owned by the retiring session.
- Browser generation changes invalidate ownership instead of guessing.
- Ownership conflicts and malformed downstream responses fail closed.
- Generic MCP routing and OpenCode routing remain separate connection modes.

## Releases

Releases are published from GitHub Actions through npm trusted publishing. Do
not add an npm token to the repository or workflow.

1. Update `package.json`, `Cargo.toml`, and `Cargo.lock` to the same version.
2. Run the Rust checks, headed E2E test, and `npm pack --dry-run`.
3. Push the release commit.
4. Create a GitHub release tagged `v<version>`.
5. The `publish` workflow builds the static binary and publishes with OIDC.
