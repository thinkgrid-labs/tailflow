# Limitations and operational boundaries

TailFlow is designed for fast local development feedback. Understanding its
boundaries prevents an agent or script from treating partial evidence as proof.

[Documentation index](README.md)

## Scope at a glance

| Area | Current boundary | Practical implication |
|---|---|---|
| Network | Loopback-only HTTP, no auth or TLS | Do not expose the daemon directly to a network |
| Retention | Bounded in-memory ring | History disappears on restart and can be evicted |
| Data type | Text logs | No traces, metrics, profiles, or state inspection |
| Classification | Text and JSON severity heuristics | Custom formats may receive the wrong level |
| Grouping | Normalized line fingerprints | Similar errors may merge; variable shapes may split |
| Docker | All running containers on the local daemon | No Compose-service allowlist, Kubernetes, or remote Docker target |
| Files | Local paths | No network log drains or object-store ingestion |
| Platforms | macOS ARM64/x64, Linux ARM64/x64, Windows x64 | Other targets require a source build and may be untested |

## Security model

The daemon binds to `127.0.0.1`, rejects nonloopback Host headers, and does not
enable permissive cross-origin access. It has no users, permissions, API keys,
or transport encryption.

That is appropriate for one developer machine. It is not sufficient for:

- a shared development server;
- direct access from another machine;
- an internet-facing endpoint; or
- multi-tenant log access.

Logs can contain credentials, tokens, personal data, and customer content.
TailFlow does not redact payloads. Configure sources carefully and remember that
any local process able to connect to the daemon can query its buffer.

If remote access is required, place an authenticated, encrypted proxy in front
of TailFlow and evaluate the exposure deliberately. Remote operation is not a
tested first-class workflow.

## Retention and completeness

The daemon retains the newest `--buffer` records, defaulting to 5,000. It does
not persist them to disk. A restart starts a new cursor lifetime with no earlier
history.

Responses expose `buffer_start_cursor`, `next_cursor`, `buffer_starts_at`, and
`cursor_gap` where relevant. Callers should treat `cursor_gap: true` or a short
buffer horizon as incomplete evidence—not as proof that nothing happened.

Increasing the buffer improves the retrospective window but uses more memory.
It does not create durable history.

## Severity inference

TailFlow prefers recognized fields in structured JSON and otherwise detects
common level tokens in text. A line without a recognized level is `unknown`,
which ranks below `trace`.

Consequences:

- custom severity names can remain `unknown`;
- prose containing words such as “error” can occasionally be ambiguous;
- stack frames normally have no severity and are recovered as trailing context,
  not independent errors; and
- a source that prints nothing can be healthy, stuck, or not yet initialized.

Always use source lifecycle state alongside error queries when deciding whether
the stack is healthy.

## Failure grouping

Deduplication replaces changing values such as numbers, timestamps, UUIDs,
addresses, and quoted strings before comparing lines. This is intentionally
lossy compression for an agent context window.

For example, missing-module errors containing different quoted module names can
group together. Conversely, the same logical failure expressed with materially
different wording can appear as separate groups. Use `search_logs` when exact
payloads or ordering matter.

## Source behavior

### Processes

Process commands run through the platform's native shell and inherit TailFlow's
working directory and environment. Relative commands and file paths are not
rebased to the directory containing `tailflow.toml`. TailFlow captures
stdout/stderr and attempts to terminate the full process tree on shutdown. It is
a development supervisor, not a production init system: it does not provide
resource limits, health checks, dependency ordering, or durable restart
accounting.

### Docker

Docker discovery follows all currently running containers and checks for
changes every two seconds. Container selection, Compose project awareness,
Kubernetes pods, and remote container runtimes are not implemented. Attaching to
a container starts with its last 50 Docker log records, so TailFlow is not a
complete container log archive.

### Files

File sources follow appends and handle a missing initial file, replacement,
rotation, and truncation. They do not recursively watch directories, expand
globs, parse compressed rotations, or read historical rotated files.

### Standard input

Piped input belongs to that TailFlow process. It cannot be recovered or shared
after the process exits unless another source also writes it to a file.

## Agent and API boundaries

- MCP uses stdio. The daemon does not yet expose MCP directly over streamable
  HTTP.
- `tailflow-mcp` does not currently start the daemon automatically.
- Queries are read-only and operate only on captured logs. TailFlow does not
  restart services or execute remediation actions through its API.
- `wait_for_logs` proves that a matching line appeared. A timeout proves only
  that no retained/new line matched before the deadline.
- The MCP output is optimized for bounded context, so callers needing exact data
  should request JSON or use the HTTP API.

## Non-goals

TailFlow is not currently intended to replace:

- production log storage and search;
- observability backends and distributed tracing;
- process managers or container orchestrators;
- test runners and CI systems; or
- security monitoring and audit logs.

These boundaries keep the product focused on its core job: giving a local coding
agent timely, compact evidence from the software it is changing.

See the [roadmap](roadmap.md) for areas that may expand over time.
