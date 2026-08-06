---
name: browser-automation
description: Use browser automation and Chrome DevTools tools to inspect or interact with websites, test web interfaces, capture screenshots, and diagnose browser console or network behavior.
---

# Browser Automation

Use the `browser_*` tools when the task requires a real, rendered browser or browser-only state such as authentication, client-side behavior, layout, accessibility, console output, or network traffic.

## Workflow

1. Navigate to the target URL, then inspect the page before interacting.
2. Use the current snapshot to identify elements and references. Refresh the snapshot after navigation or meaningful DOM changes rather than reusing stale references.
3. Perform the smallest interaction needed, then inspect the resulting page state.
4. Verify navigation, form submission, downloads, and other consequential actions from visible page state or relevant browser diagnostics.

Use screenshots when visual appearance, responsive layout, rendering, or image output matters. Prefer snapshots for locating elements and understanding page structure.

Inspect console messages for client-side exceptions and warnings. Inspect network requests when diagnosing API failures, redirects, authentication, caching, or unexpected response data.

Do not claim an interaction succeeded solely because a tool call returned without an error; verify its observable result.
