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

Use a fresh `.scratch/` directory per experiment. Record the source commit,
binary checksum, browser version, provider, exact model, and reasoning
setting. For source comparisons, give each checkout its own Cargo build
directory and reuse the shared Kache cache. Keep each condition's browser
skill.

Start `bun scripts/browser-model-eval.ts site --root=<root> --port=0` under a
known PID or held lock. Its `site.json` records the listening URL and PID. The
browser origin exposes no scoring endpoint. The host scorer reads order state
from files in the experiment directory; agents must not read those files.

Run names have a condition label and seed, such as `ours-current-1`,
`ours-guidance-2`, or `browser-use-1`. Seeds 1 and 2 select paired fixture
cases. Generate temporary project configuration with `config --root=<root>
--labels=ours-current,ours-guidance --condition=ours-guidance --seed=1
--ours-current=<binary> --ours-guidance=<binary>`. Merge it without
overwriting existing configuration. Each run gets its own profile and an
allocated CDP port. Restore the configuration and collect the fixture process
when finished.

For browser-use 1.0.0, install `browser-use==1.0.0` in a task-local Python
virtual environment. Keep the interpreter's virtual-environment path; do not
resolve its symlink. Write a JSON argv array containing that Python
executable, `scripts/browser-use-eval.py`, `--profile`, a fresh profile path,
`--ipc`, and a short task-local IPC path. Pass its path as
`--command=<argv.json> --backend=browser-use` to `config` or `mcp`.
External-command configurations require `--seed=1` or `--seed=2` to avoid
sharing one profile between active trials. The wrapper runs the official MCP
implementation with local headless Chromium and explicitly stops its browser.
Use a separate command file and profile for each seed. This dependency is for
evaluation only and is not part of the published server.

Generate each task with `prompt --root=<root> --run=ours-guidance-1
--port=<fixture-port> --skill=<condition-skill>`. Omit `--skill` for a server
that supplies its own packaged instructions. Page JavaScript, DOM operations
and normal page requests through the assigned browser MCP are permitted for
every condition. Host file/network access, fixture implementation (including
inline script contents), scoring data and other tools are forbidden. A
code-based server may use host code to orchestrate its browser helpers and
process observed page data, but cannot use it as a shortcut to the fixture's
private state.

Use separate model conversations with the same provider, model, reasoning,
harness and budgets. Counterbalance condition order across seeds. Compare the
current and candidate guidance on the same server binary before attributing
any difference to a runtime change. Test multi-harness routing separately from
single-agent task quality.

Save final answers to `<root>/answers.json` as objects containing `run`,
`answer`, `elapsedSeconds`, `thread`, optional provider `usage`, and `valid`.
Set `valid: true` only after inspecting the agent trace for forbidden tools,
source or oracle access. Run `score --root=<root>
--runs=ours-current-1,ours-guidance-1,browser-use-1`. A passing score requires
that audit, a complete trace of issued tool calls within the 50-call and
10-minute budgets, one correct order, all facts, and matching receipt data.
Setup failures and unaudited trials do not establish reliability.

The adapter records run identities independently of tool names and navigation,
so Python `browser_exec` calls and compact `dispatch` calls use the same
scorer. Reports include tool responses, client request bytes, initialization
and catalog response bytes, images, errors and supplied provider usage. Byte
counts are not tokens or billing. Include task/skill inputs and provider
token/cache/reasoning usage when available; do not infer total context or cost
from response bytes. Two paired cases are a smoke test, not a statistical
reliability estimate.

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
