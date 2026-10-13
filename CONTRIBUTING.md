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
cleanup, persistence, profile locking, or process shutdown:

```sh
cargo test --release --test e2e -- --ignored --nocapture --test-threads=1
```

The real-browser cases share Bunx's installation cache and run serially. Adapter
and tool-call concurrency is exercised within each case.

The E2E test starts adapters with temporary profiles, private sockets, and CDP
ports. It verifies automatic race-safe broker startup, shared cookies, tab
isolation, compact dispatch, concurrent calls, release during an active call,
reconnecting adapters, unowned-tab preservation, lazy Chromium activation,
per-session window placement, and automatic browser shutdown. The regular
integration suite verifies that one adapter disconnect cannot stop a broker
still used by another adapter.

Check the npm artifact when changing package files or the native binary:

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
- One auto-managed broker owns the browser configuration, profile lease, socket
  lease, workers, and browser lifecycle.
- The first adapter starts the broker. Concurrent startup must converge on the
  lease winner rather than clone a browser or profile.
- Independent adapters may share one broker and profile. Their tab ownership
  remains isolated while browser profile data is shared deliberately.
- A second broker must fail clearly rather than share or clone an active profile
  or socket.
- Compact OpenCode tools disclose one current action schema at a time without
  duplicating upstream definitions.
- Per-action OpenCode permission or tool rules force direct mode rather than
  silently broadening access through the compact dispatcher.
- Tool discovery and compact help must not activate the browser runtime.
- The broker exits only after every adapter disconnects and the reconnect grace
  expires.
- Broker shutdown must reject new calls and give active calls up to five seconds
  to finish before closing workers and the browser.
- Window layout is best-effort presentation. Stable target ownership remains
  authoritative, and layout repair must not move, claim, or close tabs.

## Releases

GitHub releases reuse the package tested by CI. Push a versioned commit to
`main`, wait for CI, then dispatch `release.yml` with its full `commit` SHA.
The workflow verifies that commit is still main and attaches the tested
package and `SHA256SUMS`.

Keep npm, Cargo, and Codex plugin versions synchronized. npm names remain
unchanged for now. npm publication is a separate, explicit `publish.yml`
dispatch with `publish_npm=true`; Browser MCP's trusted publisher must first
be updated for the renamed repository. Do not add an npm token.
