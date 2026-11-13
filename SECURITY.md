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
agent's behavior. The broker validates structured output internally before formatting bounded
model-facing text, but it cannot prevent prompt injection.

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
sensitive. The broker also masks common sensitive-name URL parameters, headers,
JSON/form fields, and configured secret values in model-facing network output.
This is heuristic redaction, not a guarantee that arbitrary secrets are removed.
Exported files, screenshots, recordings, DOM evaluation and unexpected encodings
can reveal values. These settings do not disable browser telemetry or prevent websites from
making network requests.

## Optional secrets and recordings

Secrets are loaded only from an explicitly configured, bounded, private file.
Insertion requires an exact allowed HTTPS top-level origin and editable element.
Origin checks and insertion run together; there is no origin check followed by a
separate navigation-sensitive insertion. Pages on an allowed origin can read the
value through their own scripts. Same-user CDP access can also read it.

The broker suppresses secret-fill results, removes the worker's `DEBUG`
environment variable and scrubs known values from model text and diagnostics.
These measures do not make browser content or exported artifacts safe to share.
No credential manager, secret creation or automatic recording is enabled.
Recording requires explicit opt-in and stores files in the workspace.

## Supported versions

Security fixes are provided for the latest published release. Reports against
older versions should first be reproduced on the latest version when possible.
