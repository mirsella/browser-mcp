---
name: browser-automation
description: Use browser automation to inspect or interact with websites, test rendered interfaces, capture screenshots, and diagnose console or network behavior.
---

# Browser Automation

Use these tools for rendered state, authentication, interaction, accessibility,
layout, screenshots, console output and network traffic.

OpenCode exposes `browser_help` and `browser_dispatch`; Codex namespaces `help`
and `dispatch` under this plugin. First call help without an action to discover
available names; use only listed actions. Help also loads one argument schema.
Dispatch takes `{ "action": "navigate_page", "arguments": { "url": "https://example.com" } }`.
Put every action field inside `arguments`. Load help when a signature is unclear.
Direct action tools, when present, use the same workflow. `batch` is compact-only.

The host supplies conversation identity. Never add routing fields. Codex
subagents share their parent's tabs; coordinate using explicit `page` handles.

1. Reuse the current page. Open `new_page` only for simultaneous work (default cap 5), and close temporary pages when done. New pages default to background.
2. Inspect before acting. `take_snapshot` supports `find`, `within` a UID, `interactive`, `diff`, and `maxLines` (default 300, maximum 2000). Search or scope large pages instead of repeatedly requesting the full tree. Diff cannot be combined with filters.
3. Copy UIDs exactly. They include a worker prefix; refresh after navigation, worker replacement or missing elements. File exports are diagnostic dumps with upstream UIDs; obtain actionable UIDs from an inline snapshot.
4. Prefer `fill_form` for several inputs: `arguments: {"elements":[{"uid":"<snapshot UID>","value":"text"}]}`. Values are strings; use the displayed option text for selects. Load help before guessing other argument shapes. Use `batch` for up to 8 already-understood steps, with a snapshot only on the last step. It stops on error; inspect completed/not-run counts and never replay a partially completed batch blindly.
5. Verify the observable result. A successful tool call alone does not prove the intended effect. Input actions omit snapshots unless requested; inspect when the result matters. When the visible confirmation supplies the requested facts, stop. An order ID and total in a confirmation are receipt data; a separate page labelled "Receipt" is unnecessary. Use `wait_for` for an expected change, not a guessed extra section or success screen.

`list_pages` shows session-local `p1`, `p2`, etc. `select_page` changes current;
`page` on scoped actions targets an owned page without changing current. Unknown
or closed handles fail. Selecting does not focus the window unless requested.

Scroll with `press_key` (`PageDown`, `PageUp`, `Home`, `End`). Use screenshots for visual work. Console and network lists default to 50 entries;
paginate or narrow them. Export large request/response bodies to workspace files.
Text responses are bounded to 32 KiB; truncation is explicit. Images remain intact.

For login or other user work, `handoff` start focuses the exact headed page and
pauses automation on it. End the turn and request the user's work. Finish only
after confirmation, then take a fresh snapshot. Other owned pages remain usable.

`status` observes without launching a browser or worker. CLI `doctor` diagnoses
configuration. Ordinary action errors retain healthy workers; interrupted actions
may have happened and are never replayed automatically.

`fill_secret`, when configured, accepts a secret name and input UID; it checks an
allowed HTTPS top-level origin and returns no value. Do not read the filled value.
Recording tools appear only when explicitly enabled; stop recordings before
cleanup. There is no automatic capture, HAR export, or rolling recorder.
