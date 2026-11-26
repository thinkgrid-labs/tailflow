# Security and local API access

TailFlow's daemon serves its dashboard and HTTP API on `127.0.0.1`. That
API can return agent sessions, project and file paths, commands (depending on
the capture mode), runtime logs and error details. Read-only is not the same
as harmless, so every endpoint that returns data requires a credential.

## Who can reach a loopback port

Binding to `127.0.0.1` keeps the network out, but it does not keep out:

- **other OS users** on the same machine (shared workstations, CI runners);
- **containers sharing the host network** (`--network host`), which cannot
  read your home directory but can open `127.0.0.1:7878`;
- **web pages in your browser**, including other localhost dev servers and
  anything they load.

All three are refused unless they hold a credential.

## Credentials

| Client | Credential | How it gets it |
|---|---|---|
| `tailflow-mcp`, `tailflow-logs`, scripts | API token (`Authorization: Bearer …`) | Read automatically from the daemon's runtime file, or `$TAILFLOW_TOKEN` |
| Browser dashboard | `HttpOnly; SameSite=Strict` session cookie | A one-time sign-in link |

### API token

The daemon generates a 256-bit token from the OS random source every time it
starts. It writes the token to an owner-only runtime file:

| OS | Runtime file |
|---|---|
| macOS | `~/Library/Application Support/tailflow/run/daemon-<port>.json` |
| Linux | `$XDG_DATA_HOME/tailflow/run/daemon-<port>.json` (default `~/.local/share/…`) |
| Windows | `%LOCALAPPDATA%\tailflow\run\daemon-<port>.json` |

`$TAILFLOW_DATA_DIR` overrides the directory. On Unix the file is `0600` in a
`0700` directory. On Windows it inherits the per-user ACL of
`%LOCALAPPDATA%`. The file is removed on a clean shutdown.

Local clients running as the same user need no setup. `tailflow-mcp` and
`tailflow-logs` find the token for the port in `$TAILFLOW_URL`, and pick up a
new token on their own after the daemon restarts.

Other clients:

```sh
# curl and scripts
curl -H "Authorization: Bearer $(tailflow-daemon token)" localhost:7878/api/v1/sessions

# A container, another user, or an SSH port forward: pass the token explicitly
TAILFLOW_TOKEN=$(tailflow-daemon token) tailflow-logs errors
```

Starting the daemon with `TAILFLOW_TOKEN` set (32–256 characters from
`A–Z a–z 0–9 - _ . ~`) makes it use that token instead of a random one. This
is useful for CI and containers. An invalid value stops the daemon rather
than being replaced silently.

### Browser sign-in

When the daemon starts it prints a one-time sign-in link:

```
TailFlow v0.4.0 · daemon
  dashboard   http://127.0.0.1:7878/auth/login/3f9c…
              one-time link, valid 60 min · later: tailflow-daemon dashboard
```

Opening it sets the session cookie and redirects to the dashboard. The link
works once. Later, or in another browser, run:

```sh
tailflow-daemon dashboard          # opens the browser with a fresh link (valid 5 minutes)
tailflow-daemon dashboard --no-open  # prints the link instead
tailflow-daemon dashboard --port 7879
```

Only a holder of the API token can create sign-in links. There is no endpoint
that hands out the token or a session to a caller without a credential.

A browser session:

- is shared by every tab of that browser;
- survives daemon restarts on the same port, so open dashboards reconnect;
- ends after 12 hours without use, after 7 days, on **Settings → Sign out**,
  or for every browser at once with
  `tailflow-daemon dashboard --revoke-sessions`.

Sessions are stored next to the runtime file
(`run/sessions-<port>.json`, owner-only). The API token is never persisted
across runs.

## Browser protections

Browsers send `127.0.0.1` cookies to every port on `127.0.0.1`, and pages on
other localhost ports can set cookies there. Port is not a boundary for
cookies, so the daemon also checks where each browser request came from.

- A session cookie is accepted only when the browser reports the request as
  `Sec-Fetch-Site: same-origin` (the dashboard itself) or `none` (a URL you
  typed or opened from a terminal). A page on `127.0.0.1:3000` is
  `same-site`, and its requests are refused even though the cookie is
  attached. That covers `fetch`, `EventSource`, `<img>` and `<script>`.
- If an `Origin` header is present, it must name the dashboard's origin
  exactly.
- The cookie name includes the port (`tailflow_session_7878`), so two
  daemons don't overwrite each other's sessions. Several cookies with that
  name are tolerated, so a cookie set by another port can't shadow yours.
- There are no CORS headers, so responses are unreadable from any other
  origin.
- `Host` must be a loopback name. This defeats DNS rebinding even with a
  valid credential.
- Every protected path refuses an unauthenticated request with the same
  answer, whether or not the resource exists, so it can't be used to probe
  for sessions or files.
- The dashboard can't be framed (`frame-ancestors 'none'`). It loads only
  its own scripts (strict CSP) and never sees the session secret
  (`HttpOnly`).
- A sign-in link opened by another web page (`Sec-Fetch-Site: same-site` or
  `cross-site`) is refused and is not used up.

Browsers that don't send Fetch Metadata can't use the dashboard. That means
Chrome before 76, Firefox before 90 and Safari before 16.4.

## What is public

Only these paths work without a credential:

- the dashboard's static files (HTML, JS and CSS, with no data);
- `/health`, which returns `{"ok": true, "version": …, "auth_required": true}`
  without a credential and the full status with one;
- `/auth/login/<code>`.

Everything else needs a credential, including paths that don't exist. A new
endpoint can't ship unauthenticated by accident.

## Outside the threat model

- **Processes running as your own OS user.** They can read the runtime file
  and the database directly. Use OS accounts or sandboxes to separate
  untrusted code.
- **A malicious server on another loopback port that your browser visits.**
  Because cookies ignore ports, that server receives your TailFlow session
  cookie and could replay it outside a browser. On a single-user machine this
  is equivalent to the previous item. Sign out, or revoke sessions, if you
  suspect it.
- **Network exposure.** The daemon serves plain HTTP. If you need remote
  access, put an authenticated, encrypted proxy in front of it, or use an SSH
  tunnel with `TAILFLOW_TOKEN`.

## Upgrading from 0.3.x

The API now requires a credential.

- `tailflow-mcp` and `tailflow-logs` from the same release handle this
  automatically. Older binaries get `401` and need upgrading together with the
  daemon.
- `curl` and scripts must send `Authorization: Bearer $(tailflow-daemon token)`.
- `/health` without a credential now returns liveness only.
- The dashboard asks you to sign in once per browser, using the link the
  daemon prints or `tailflow-daemon dashboard`.
- `tailflow-logs` has a new exit code: `4` means not authorized.
