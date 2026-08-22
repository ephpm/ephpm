+++
title = "GitHub OAuth Gate (native middleware)"
weight = 11
+++

> **Status.** The module is implemented and tested. The complete login has
> been driven over real HTTP through a running ePHPm against a **stub** GitHub;
> **a round trip against the real `github.com` with a registered GitHub App has
> not been performed** and needs a human. See
> [Verification status](#verification-status) — nothing on this page is claimed
> beyond what was actually observed.

`github-auth` gates a private preview site on **GitHub identity**: *does the
person at this browser have access, on GitHub, to the repository (or org, or
team) this preview belongs to?*

It is a [dynamic (`dlopen`) middleware module](/guides/native-middleware/),
not a builtin — it needs an HTTP client and a TLS stack, and none of that
belongs in the `ephpm` binary.

## It is half of a pair

Two jobs, deliberately in two modules:

| | **`github-auth`** (this module) | a session-verifier module |
|---|---|---|
| Runs on | login, the OAuth callback, and the first request of a session | every request |
| Talks to GitHub | yes, 3 calls | **never** |
| Owns | issuing sessions | the token format and its verification |
| When unhappy | `RESPOND` 302 → GitHub | `RESPOND` 302 → `login_path` |

**Mounted alone, `github-auth` is not an authenticator.** It stops a request
that carries *no* session cookie, but it deliberately does **not** verify one
that does — that is the verifier's job, and doing it in two places would give
you two subtly different verifiers. The module logs this at startup on its
first request.

```toml
[[middleware]]
library = "/opt/ephpm/modules/libgithub_auth.so"
order   = 10                     # cold path — owns the reserved paths
config  = { client_id = "Iv1.0123456789abcdef",
            client_secret  = "env:GH_CLIENT_SECRET",
            session_secret = "env:EPHPM_SESSION_SECRET",
            repo = "acme/web" }

[[middleware]]
library = "…session verifier…"    # hot path — verifies, redirects to login
order   = 20
config  = { session_secret = "env:EPHPM_SESSION_SECRET",
            login_path = "/_ephpm/auth/github/login" }
```

## What happens per request

| Request | Verdict | Cost |
|---|---|---|
| no session cookie | 302 → GitHub `authorize`, `Set-Cookie` state | no network |
| `GET {callback_path}?code=…&state=…` | 302 → where you were, `Set-Cookie` session | 3 GitHub calls |
| `GET {login_path}` | 302 → GitHub `authorize` | no network |
| valid bypass token, no cookie | 302 → same URL, `Set-Cookie` session | no network |
| session cookie present | `CONTINUE` — the verifier decides | **no network** |

PHP does not run on any redirect row: the middleware chain is evaluated before
PHP dispatch and before the request body is read.

**A GitHub API call is never made on a request that already has a session.**
That is structural, not a discipline: only the exact configured
`callback_path` can reach the network, and the routing decision is a pure
function with its own tests.

## Requirements

**Use a GitHub App, not a classic OAuth App.** With an OAuth App, seeing a
private repository at all requires the `repo` scope — read *and write* on
every repository the user can touch, handed to a preview host. A GitHub App's
user-to-server token is bounded by the app's installation instead, and needs
no scope for the repository check. That is why `scopes` defaults to empty.

**The reserved paths must reach PHP routing.** ePHPm evaluates middleware on
the PHP-bound path, so `login_path` and `callback_path` must resolve to a PHP
script for the module to see them. With the default
`fallback = ["$uri", "$uri/", "/index.php?$query_string"]` and a front
controller at `index.php` — WordPress, Laravel, Symfony — that is automatic,
and PHP still never runs because the chain short-circuits first. A site with
no `index.php` and a `=404` fallback would 404 the callback before the module
is consulted.

**`dlopen` must work.** The stock Linux release is glibc-dynamic and can load
this. A custom fully static (musl) ePHPm **cannot `dlopen` at all** and
therefore cannot use this module; such a build would need it compiled in as a
builtin instead.

## Build and mount

```bash
cargo build --release -p ephpm-middleware-github-auth
# target/release/libgithub_auth.so    (Linux)
# target/release/libgithub_auth.dylib (macOS)
# target/release/github_auth.dll      (Windows)
```

`library` must be a **path** (something containing a separator or an
extension). A bare name is resolved against the compiled-in builtin registry
first, so it would not load this module.

## Configuration

Secrets accept an **`env:NAME`** indirection, read once at startup. Prefer it:
`ephpm.toml` is often templated or rendered into a ConfigMap, and a literal in
it is a secret in git.

| key | default | meaning |
|---|---|---|
| `client_id` | **required** | OAuth client id of the GitHub App / OAuth App |
| `client_secret` | **required** | its client secret; `env:NAME` supported |
| `session_secret` | **required**, ≥ 32 bytes | HMAC key for issued sessions. Must match the verifier's |
| `check` | `"repo"` | `"repo"`, `"org"` or `"team"`. Inferred when unambiguous |
| `repo` | — | `owner/name`, for `check = "repo"` |
| `org` | — | organisation login, for `check = "org"` / `"team"` |
| `team` | — | team slug, for `check = "team"` |
| `sites` | unset | table mapping each vhost to its own `{ repo \| org \| team }` |
| `login_path` | `/_ephpm/auth/github/login` | reserved: starts a login |
| `callback_path` | `/_ephpm/auth/github/callback` | reserved: receives GitHub's redirect |
| `redirect_uri` | derived | full callback URL sent to GitHub. Defaults to `https://<vhost><callback_path>` |
| `cookie_name` | `ephpm_session` | session cookie. Must match the verifier's |
| `state_cookie_name` | `<cookie_name>_oauth` | short-lived OAuth `state` cookie |
| `cookie_path` | `/` | `Path` attribute on both cookies |
| `cookie_secure` | `true` | emit `Secure`. Turn off only for a plaintext local preview |
| `cookie_samesite` | `"Lax"` | `Lax`, `Strict` or `None` (`None` requires `cookie_secure`) |
| `session_ttl_secs` | `28800` (8 h) | 60 – 604800 |
| `issuer` | `ephpm-github-auth` | `iss` claim |
| `audience` | unset | `aud` claim |
| `scopes` | `[]` | OAuth scopes. Empty is correct for a GitHub App |
| `bypass_token` | unset | pre-shared automation token, ≥ 32 bytes |
| `bypass_header` | `x-ephpm-bypass` | header carrying it |
| `bypass_query` | unset | query parameter carrying it — **off unless set** |
| `bypass_ttl_secs` | `3600` | lifetime of a bypass-issued session |
| `github_base` | `https://github.com` | authorize/token host (a GHES host works) |
| `github_api_base` | `https://api.github.com` | REST base (`https://ghes/api/v3`) |
| `http_timeout_secs` | `10` | 1 – 60, per outbound call |

`github_base` / `github_api_base` must be `https://`, with one exception:
`http://` is accepted for a **loopback** host, which is what lets the test
suite point the module at a stub server. Plaintext to a real GitHub host is
refused outright — it would put the client secret and the user's access token
on the wire in clear.

If `sites` is present it is **authoritative**: a vhost with no entry gets no
access at all, even when a top-level `repo` is also configured. A hostname
nobody mapped must not quietly inherit another tenant's rule.

## Which GitHub calls are made

Per login, once:

| # | Call | Purpose | Minimum permission |
|---|---|---|---|
| 1 | `POST {github_base}/login/oauth/access_token` | code → user access token | none — this *is* the grant |
| 2 | `GET /user` | the login that becomes `sub` | GitHub App: none. OAuth App: `read:user` |
| 3a | `GET /repos/{owner}/{name}` | `check = "repo"` | GitHub App: `metadata: read`. OAuth App: `repo` |
| 3b | `GET /user/memberships/orgs/{org}` | `check = "org"` — must be `active` | `read:org` |
| 3c | `GET /orgs/{org}/teams/{team}/memberships/{login}` | `check = "team"` — must be `active` | `read:org` |

Redirects are never followed. A visible consequence: a **renamed** repository
stops matching until the config is updated, which is the safe direction.

The access token is used for those calls and then dropped. It is never stored,
never written to a cookie, and never logged.

## The session

An HS256 JWT — the same shape the builtin `jwt` module already verifies, down
to `alg` pinning and a mandatory `exp`.

| claim | meaning |
|---|---|
| `iss` | `issuer` |
| `sub` | GitHub login, or `automation` for a bypass session |
| `aud` | `audience`, when configured |
| `iat` / `exp` | issue / expiry, seconds since the epoch |
| `site` | the vhost this session is for |
| `via` | `github`, `bypass` — or `share` for an externally minted token |
| `gh_id` | numeric GitHub account id (stable across renames) |
| `check` | the rule that was satisfied, e.g. `repo acme/web` |

There is **no server-side session record**. Restarts do not log anyone out, a
second node needs no replication, and the module never touches the middleware
KV surface (which is process-global rather than per-site — issue #376).

The cost, plainly: **an issued session cannot be revoked before it expires.**
Rotating `session_secret` invalidates every session at once and is the only
revocation available. `session_ttl_secs` is therefore the blast radius of a
stolen cookie.

### Cookie attributes, and why

`HttpOnly` (the token *is* the session, and a preview site is pre-production
code — exactly where XSS lives), `Secure` (a bearer credential must not ride a
plaintext hop), `SameSite=Lax` (`Strict` would bounce a user through GitHub
every time they clicked a preview link from a PR comment or Slack; `Lax` sends
the cookie on top-level GET navigations, which is that case *and* the OAuth
callback), `Path` (the gate covers the whole site), a real `Max-Age` matching
the token's `exp` — and **no `Domain`**, which makes the cookie host-only.
`Domain=.preview.example.com` would send one tenant's session to every other
tenant on the wildcard. That one is not configurable.

## Automation bypass

Playwright, Lighthouse and smoke tests need in. Set `bypass_token` (≥ 32
bytes) and present it:

```bash
curl -sL -c jar.txt -H "x-ephpm-bypass: $EPHPM_BYPASS" https://pr-123.preview.example.com/
lighthouse https://pr-123.preview.example.com/ \
  --extra-headers "{\"x-ephpm-bypass\":\"$EPHPM_BYPASS\"}"
```

Presenting it yields *the same kind of session* — a 302 and a `Set-Cookie` —
not a per-request pass-through. The client follows one extra redirect on its
first request and behaves normally afterwards. The comparison is constant-time
and independent of length; a wrong token behaves exactly like no token at all
(302 to GitHub), so the header is not an oracle.

`bypass_query` exists for tools that cannot set headers, and is **off unless
configured**: a query string lands in access logs, browser history and
`Referer` headers, which a header does not.

## Share links

Because issuance and verification are separate, and issuance is not
privileged, **a hosting control plane can mint its own sessions with no change
to this module.** Anything holding `session_secret` can produce an acceptable
token: same HS256 JWT, `via = "share"`, whatever `sub` and `exp` you want.

```php
$claims = ['iss' => 'ephpm-github-auth', 'sub' => 'share:designer@example.com',
           'site' => 'pr-123.preview.example.com', 'via' => 'share',
           'iat' => time(), 'exp' => time() + 7200];
$b64 = fn($v) => rtrim(strtr(base64_encode($v), '+/', '-_'), '=');
$signing = $b64('{"alg":"HS256","typ":"JWT"}') . '.' . $b64(json_encode($claims));
$token = $signing . '.' . $b64(hash_hmac('sha256', $signing, $secret, true));
// then: Set-Cookie: ephpm_session=$token; Path=/; Max-Age=7200; HttpOnly; Secure; SameSite=Lax
```

This is a real capability and a real risk: `session_secret` is a minting key.
Treat it like one.

## Security properties

**CSRF on the callback is enforced.** A login mints a 32-byte random nonce,
puts it in the `state` parameter *and* in an `HttpOnly` signed cookie bound to
the vhost and expiring in 10 minutes. The callback requires both, compares
them in constant time, and refuses when the cookie is missing, unsigned,
expired, or issued for another host. An attacker who plants their own
`code`+`state` in a victim's browser cannot supply the matching cookie.

**Return-to is constrained to a same-origin path.** Absolute URLs,
scheme-relative `//evil.test`, backslash forms, `javascript:`/`data:`, raw
control characters, non-ASCII, `..` segments and over-long values all collapse
to `/`. Values are re-validated after being read back out of the signed state
cookie — a signature attests origin, not safety.

**Constant-time comparison** is used for every secret and signature (HMAC
verification for tokens; an HMAC-of-both comparison for the bypass token, so
the timing leaks neither the contents nor the length).

**Nothing secret is logged.** No code, token, secret or minted session appears
in any log line; the error type physically cannot carry one. GitHub's OAuth
error identifiers are filtered to `[A-Za-z0-9_]` before being logged. Refusal
pages are plain text with no interpolation, so the gate cannot become a
reflected-XSS surface on the preview's own origin.

**Fail-closed everywhere.** GitHub unreachable → 502. A vhost with no
configured target → 403. A panic anywhere in the module → 500 (the `declare!`
macro's containment). None of these fall through to PHP.

### The client secret lives on the node

This is the real cost of doing OAuth in-process instead of in a separate
identity service. The OAuth **client secret** is present on every node serving
previews, readable by the ePHPm process.

If a node is compromised the attacker can impersonate the OAuth app — complete
an authorization flow for any user who can be induced to click an authorize
link — and, holding `session_secret`, mint sessions for any user and any site
this gate protects. They cannot read a user's GitHub data without that user
authorizing, and no long-lived GitHub token is stored: the access token is
used during the login and dropped. **Rotate `client_secret` and
`session_secret` together after any node compromise.**

## Limitations

- **One callback host per app.** GitHub does not support wildcard callback
  URLs. A fleet of per-PR hostnames needs a single auth origin and a
  cross-host hand-off; that is **not implemented here**. Today, `redirect_uri`
  is derived per request from the vhost, which works when each preview host is
  registered (or when one host is used).
- **No revocation before expiry.** See [The session](#the-session).
- **The `state` cookie is replayable within its 10-minute window** by whoever
  holds it — i.e. by the user themselves. Reuse of an authorization *code* is
  refused by GitHub, not here.
- **No `dlopen`, no module** — see [Requirements](#requirements).

## Verification status

| Behaviour | How it was verified |
|---|---|
| **The whole login, cold: no cookie → 302 → callback → 3 GitHub calls → session → the app** | **real HTTP through a running ePHPm with the module `dlopen`ed**, against a stub GitHub in its own process, driven with `curl` and a cookie jar |
| The callback emits **both** `Set-Cookie` headers (session issued, `state` cleared) | same run — two `set-cookie` lines on the wire |
| No cookie → 302 to GitHub carrying a `state`, PHP never runs | same run: `content-length: 0`, no PHP output; the same URL *with* a cookie reaches PHP |
| Session cookie → `CONTINUE`, PHP dispatched, gate adds no headers | same run |
| A request with a session makes **zero** outbound calls | 21 live requests after login left the stub GitHub's log unchanged; the integration test asserts the same across 50 |
| Callback with missing / forged / empty / wrong-host / expired `state` → 400 | real HTTP + unit tests |
| Hostile return-to values (27 shapes) collapse to `/` | real HTTP, decoding the signed state cookie |
| Bypass token accepted; a wrong one is indistinguishable from none | real HTTP |
| Code exchange, `/user`, and all three access checks | integration test against a stub GitHub over real sockets, using the module's own reqwest/rustls client |
| Denials: no repo access, `pull: false`, `pending` org membership, missing team | integration test |
| GitHub unreachable → 502, no session issued | integration test |
| **A round trip against real `github.com` with a real GitHub App** | **not done — needs a human.** Register an App, set `client_id`/`client_secret`, log in with a browser, confirm the `Set-Cookie` and the landing page |
