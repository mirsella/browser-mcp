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

For a headless Linux terminal, wrap the real-browser command in `xvfb-run -a`
to exercise headed handoffs and window placement. The compact workflow case
covers tab selection, UID lifetime, filtered snapshots, diffs, batch failure
boundaries and human handoffs. `actions::tests::context_budgets_and_pinned_schema_dialect`
prints static schema/help byte counts; these are not model-success measurements.
The public pinned tool-schema fixture comes from Chrome DevTools MCP 1.6.0.

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

## Model browser evaluation

`scripts/browser-model-eval.ts` provides a local dummy shop, a transparent MCP
trace adapter, prompts, configuration, and an independent scorer. It exercises
a 221-product catalog, popup details, form entry, discount arithmetic, and a
checkout that fails its first valid submission. No money or external account
is involved. `bun test tests/model-eval.test.ts` checks the fixture's rejection
and retry behavior.

Use a fresh `.scratch/` directory per experiment. Build the baseline from a
recorded commit in a separate source directory and copy the two release
binaries to `<root>/bin/before` and `<root>/bin/after`. Keep each build's own
browser skill. Start `bun scripts/browser-model-eval.ts site --root=<root>`
under a known PID or held lock; stop and collect it when the experiment ends.

Generate temporary project MCP configuration with `config --root=<root>
--condition=before` (then `after`) and merge it without overwriting existing
configuration. Generate each prompt with `prompt --run=before-1
--skill=<baseline-skill-path>`, repeating for `before-2`, `after-1`, and
`after-2`. Use separate model conversations with the same provider, model and
effort. Run both baseline cases before switching configuration. The adapter
preserves trusted OpenCode session metadata; the model never supplies routing.

Save final answers to `<root>/answers.json` as objects containing `run`,
`answer`, `elapsedSeconds`, and `thread`, then run `score --root=<root>`.
Scoring checks one correct order and all reported facts and confirmation data.
Traces retain tool calls, errors, text bytes, structured bytes, and total MCP
response bytes. These byte counts are not tokenizer counts or provider billing.
Inspect the agent traces for forbidden tools or source access before accepting
a result. Exclude setup failures explicitly. Restore project configuration and
stop only the experiment's adapters, workers and browsers afterward. Two cases
per version are a smoke test, not a statistical reliability estimate.

## Releases

Push the versioned source commit to `main` for 2.x, or to
`release/opencode-v1` for 1.x, and wait for CI. Dispatch `release.yml` from
`main` with the full source `commit` SHA. It verifies the pinned workflow and
source branch, then reuses the checksummed `npm-package` artifact.

Configure npm's trusted publisher as this repository's `release.yml` without
an environment. Dispatch with `npm_mode=publish` to publish through OIDC,
create the GitHub release, and update the Codex marketplace branch for `latest`.
Use `npm_mode=dry-run` to verify without writing. Versions 1.x use `opencode1`;
2.x use `latest`. GitHub releases attach the same tested tarball and
`SHA256SUMS`. Keep npm, Cargo and Codex versions synchronized.

CI runs headed tests against Chrome for Testing 155.0.8059.39. Update the pin
when validating a new browser version.
