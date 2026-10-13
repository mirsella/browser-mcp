# Security policy

## Reporting a vulnerability

Please use [GitHub private vulnerability reporting](https://github.com/mirsella/browser-mcp/security/advisories/new).
Do not open a public issue for a vulnerability that exposes browser data,
credentials, local files, or cross-session tabs.

Include the package version, browser and version, MCP host, reproduction steps,
and whether the issue also reproduces with `chrome-devtools-mcp@1.6.0` directly.

## Security boundary

This project isolates tab ownership between cooperating MCP sessions. It is not
a sandbox for mutually untrusted local users or processes.

The broker keeps its Unix socket, profile lock, and ownership state private to
the current Unix user, but the managed browser exposes an unauthenticated CDP
endpoint on loopback. Any process running as the same user can connect to that
port and control the browser without going through the ownership broker.

Only one broker may open a given browser profile. The profile and socket locks
prevent accidental concurrent use and profile corruption; they are not a
security boundary against another process running as the same user. Multiple MCP
adapters are expected to connect to that broker and share its browser profile.

The broker socket accepts only peers with the broker's Unix UID and is created
with mode `0600` in a private directory. This protects against other local users,
not against another process already running as the same user.

All browser sessions share one user-data directory. This includes cookies,
authentication, local storage, cache, extensions, permissions, history, and
downloads. A session cannot inspect another session's tabs through the broker,
but websites can still communicate through shared browser data and origins.
The optional per-session window layout is visual organization only and does not
create another security boundary.

Browser content is untrusted input. Pages can include text designed to alter an
agent's behavior. The broker preserves structured output where upstream tools
provide it, but it cannot prevent prompt injection.

## Operational guidance

- Use a dedicated browser user-data directory for sensitive automation.
- Do not bypass or remove the profile or socket locks to run another broker.
- Let the packaged adapters share the auto-managed broker through its private
  socket. Do not start another browser against the same profile.
- Do not expose the CDP port beyond loopback.
- Do not run untrusted local processes while a sensitive profile is open.
- Use OS, container, or VM isolation when local users or workloads do not trust each other.
- Keep OpenCode, the browser, this package, and `chrome-devtools-mcp` updated.
- Review browser downloads and file writes before opening or executing them.
- Treat screenshots, DOM text, console messages, and network responses as untrusted data.

The downstream worker runs with usage statistics and CrUX requests disabled.
It also disables update checks and redacts headers that upstream classifies as
sensitive. URLs, bodies, and other headers can still appear in tool output.
These settings do not disable browser telemetry or prevent websites from
making network requests.

## Supported versions

Security fixes are provided for the latest published release. Reports against
older versions should first be reproduced on the latest version when possible.
