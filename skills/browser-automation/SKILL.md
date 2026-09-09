---
name: browser-automation
description: Use browser automation to inspect or interact with websites, test rendered interfaces, capture screenshots, and diagnose console or network behavior.
---

# Browser Automation

Use the browser tools when the task requires rendered state, authentication,
client-side behavior, layout, accessibility, console output, or network traffic.

OpenCode normally exposes two compact tools:

- `browser_help` lists action names or loads the current schema for one action.
- `browser_dispatch` executes an action with `{ action, arguments }`.

For compact `browser_dispatch`, put every action-specific field inside `arguments`,
not beside it. For example, use `{ "action": "navigate_page", "arguments": { "url": "https://example.com" } }`.

Common actions include `navigate_page`, `take_snapshot`, `click`, `fill_form`,
`take_screenshot`, `evaluate_script`, `list_console_messages`, and
`list_network_requests`. Call `browser_help` for an action before using it when
its exact arguments are not already clear. Do not guess argument shapes.

Direct `browser_*` action tools may appear when compact tools are disabled. Use
them directly with the same workflow.

## Workflow

1. Reuse the current page by default. Use `navigate_page` to change URLs or reload after code or deployment changes. Use `new_page` only when simultaneous pages are necessary (max 5 owned pages per session; extra `new_page` calls fail, so `close_page` a finished page first), and close temporary pages when finished.
2. Navigate, then inspect the page before interacting.
3. Use the latest snapshot to identify elements and references. Refresh it after navigation or meaningful DOM changes.
4. Perform the smallest interaction needed, then inspect the result.
5. Verify consequential actions from visible state or relevant diagnostics.

Use screenshots when visual appearance, responsive layout, rendering, or image output matters. Prefer snapshots for locating elements and understanding page structure.

Inspect console messages for client-side exceptions and warnings. Inspect network requests when diagnosing API failures, redirects, authentication, caching, or unexpected response data.

Do not claim an interaction succeeded solely because a tool call returned without an error; verify its observable result.
