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

> **Where it lives.** This module is maintained in its own repository,
> [`github.com/ephpm/middleware-github-auth`](https://github.com/ephpm/middleware-github-auth),
> not in the ePHPm core workspace, and is released from there. See
> [Build and mount](#build-and-mount) for how to obtain the `.so`.

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

The verifier this is designed to pair with is the **`session-cookie` builtin**
(ephpm#388) — same HS256 verification as the `jwt` builtin, but reading a
cookie and redirecting instead of answering `401`:

```toml
[[middleware]]
library = "/opt/ephpm/modules/libgithub_auth.so"
order   = 10                     # cold path — owns the reserved paths
[middleware.config]
client_id      = "Iv1.0123456789abcdef"
client_secret  = "env:GH_CLIENT_SECRET"   # this module resolves env:NAME itself
# The literal session key — byte-for-byte the same string as the verifier's
# `secret` below. Generate it once, e.g. `openssl rand -hex 32` (>= 32 bytes).
session_secret = "REPLACE-ME-with-a-random-key-of-at-least-32-bytes"
repo           = "acme/web"

[[middleware]]
library = "session-cookie"       # hot path — verifies, redirects to login
order   = 20
[middleware.config]
# LITERAL. The session-cookie builtin does NOT expand env:/file: — see below.
secret          = "REPLACE-ME-with-a-random-key-of-at-least-32-bytes"
cookie          = "ephpm_session"
login_url       = "/_ephpm/auth/github/login"
return_to_param = "next"
```

> **Do not write `secret = "env:…"` on the `session-cookie` side.** This
> module resolves `env:NAME` in its own config, but the built-in
> `session-cookie` (like `jwt` and `preview-gate` mounted through
> `[[middleware]]`) uses `secret` **literally**. `secret =
> "env:EPHPM_SESSION_SECRET"` there makes the HMAC key the public string
> `env:EPHPM_SESSION_SECRET`: every real session this module mints is refused,
> and anyone who has read these docs can forge a session that verifies.
> Earlier versions of this page showed exactly that line — if you copied it,
> replace it and rotate. The verifier's `secret` must be the same bytes as the
> issuer's resolved `session_secret`; you may keep `session_secret =
> "env:NAME"` on the issuer side as long as the variable holds exactly the
> string you put in `secret` (the issuer does not trim it). Protect the
> literal by rendering `ephpm.toml` from your secret store at deploy time and
> making it readable only by the service user. On a `sites_dir` preview host
> the per-site [`[preview_auth]`](/guides/virtual-hosts/#preview_auth-the-access-gate)
> override is the one ePHPm-side verifier config that **does** accept
> `env:`/`file:` references (and enforces ≥ 32 bytes). See
> [Secrets in `config` are literal](/guides/native-middleware/#secrets-in-config-are-literal).

Three things must line up, and only three:

| this module | `session-cookie` | note |
|---|---|---|
| `session_secret` | `secret` | the same HMAC key — that is what makes the token verify. `env:` works only on this module's side; `secret` is literal |
| `cookie_name` (`ephpm_session`) | `cookie` (`ephpm_session`) | defaults already match |
| `login_path` | `login_url` + `return_to_param = "next"` | this module reads the return-to from `?next=` and sanitizes it |

`issuer` / `audience` are optional on both sides; set them on both or neither.

### The per-tenant `site` binding is automatic (issue #396)

On a multi-tenant preview host you do **not** configure the tenant scope on
either side — it is derived. This module stamps every session it mints with a
`site` claim equal to the request's **canonical site key** (`req.vhost_id()` —
the one tenant identity the router resolves), and the `session-cookie` verifier
requires that claim to equal the site key of the request it is gating
(`require_site = true`, its default). Because both read the *same* router-derived
key, they agree by construction: a session minted for one preview cannot open
another. Do not set `require_site = false` on a preview host — that turns the
binding off and reintroduces the cross-tenant replay #396 fixed. The only place
it belongs is a genuinely single-tenant deployment whose issuer mints site-less
sessions.

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

**Keep the reserved paths under `/_ephpm/auth/`.** The router hands every
request in the `/_ephpm/auth/` sub-namespace straight to the global
middleware chain, before static-file or PHP resolution — so the default
`login_path` and `callback_path` work on any site, with or without an
`index.php` front controller or a `=404` fallback. If no mounted module
answers such a path (or no middleware is mounted at all), the router returns
`404`: the namespace is reserved and nothing else serves there. The per-site
`preview-gate` verifier is deliberately not run on that namespace, so the
login round trip is never redirected back to login. A custom `login_path` /
`callback_path` **outside** `/_ephpm/auth/` gets no such routing: it is only
seen by the chain if the request reaches the normal static/PHP path, and a
per-site gate would then need it in `exempt_paths`.

**`dlopen` must work.** The stock Linux release is glibc-dynamic and can load
this. A custom fully static (musl) ePHPm **cannot `dlopen` at all** and
therefore cannot use this module; such a build would need it compiled in as a
builtin instead.

## Build and mount

This module lives in its **own repository**,
[`github.com/ephpm/middleware-github-auth`](https://github.com/ephpm/middleware-github-auth),
separate from ePHPm core — it is stable, preview-site infrastructure, and it
carries an HTTP/TLS stack that has no place in the ePHPm binary. It is **no
longer built or shipped by ePHPm releases**; get it from that repo's releases.

Each tagged release there attaches a glibc-dynamic
`github-auth.linux-x86_64.so` covered by `SHA256SUMS`, built against a pinned
ePHPm ABI commit (major 1). Download it, verify it, and place it where the
loader looks (or point `library` at it by path):

```bash
sha256sum -c SHA256SUMS
install -Dm755 github-auth.linux-x86_64.so \
  /usr/local/lib/ephpm/middleware/github-auth.linux-x86_64.so
```

With the file installed under a searched directory you can mount it by the bare
name `library = "github-auth"` (the loader tries `github-auth.<os>-<arch>.<ext>`
first). To build it yourself — or for macOS/Windows/arm64, which the repo does
not pre-build — clone that repo and run `cargo build --release` (the artifact is
`target/release/libgithub_auth.{so,dylib}` / `github_auth.dll`); use a source
revision whose pinned ABI **major** matches your ePHPm host.

`library` may be a **path** (something containing a separator or an extension)
or, once the file is installed under a searched directory, the bare name
`github-auth`. A bare name is resolved against the compiled-in builtin registry
first (which has no `github-auth` entry), then the platform-suffixed file
`github-auth.<os>-<arch>.<ext>` in the search directories.

## Configuration

This module's secrets (`client_secret`, `session_secret`, `bypass_token`)
accept an **`env:NAME`** indirection, read once at startup (the value is used
as-is — not trimmed; `file:` is not supported). Prefer it: `ephpm.toml` is
often templated or rendered into a ConfigMap, and a literal in it is a secret
in git. This is a feature **of this module's own config parser**, not of
ePHPm: the built-in verifiers you pair it with take their `secret` literally —
see the warning under [It is half of a pair](#it-is-half-of-a-pair).

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
| `endpoints_only` | `false` | passive-issuer mode: serve only login/callback and let unauthenticated **content** through (`CONTINUE`) instead of redirecting it to login. A session cookie, the bypass, and the two reserved paths behave exactly as in the default mode, so the login flow still runs and a target is still required. Use it for one global issuer alongside per-site verifiers (see [Passive issuer for public-open, private-gated fleets](#passive-issuer-for-public-open-private-gated-fleets)) |
| `access` | `"fixed"` | `"fixed"`: the configured target (`default_check`/`sites`) is authoritative — today's behaviour. `"per-preview"`: authorize each preview against the `owner/name` the router seals into the OAuth state from its `[preview_auth] repo` override; `default_check`/`sites` become optional and a request with no repo fails closed |
| `login_path` | `/_ephpm/auth/github/login` | reserved: starts a login. The router routes the `/_ephpm/auth/` sub-namespace to the middleware chain, so this default is reachable; keep custom values under `/_ephpm/auth/` |
| `callback_path` | `/_ephpm/auth/github/callback` | reserved: receives GitHub's redirect. Register `https://<host>/_ephpm/auth/github/callback` as the GitHub OAuth App's Authorization callback URL |
| `redirect_uri` | derived | full callback URL sent to GitHub. Defaults to `https://<vhost><callback_path>`. **Set it to a fixed apex** for the wildcard-fleet flow (one App, one callback host) — see below |
| `cookie_name` | `ephpm_session` | session cookie. Must match the verifier's |
| `state_cookie_name` | `<cookie_name>_oauth` | short-lived OAuth `state` cookie |
| `cookie_path` | `/` | `Path` attribute on both cookies |
| `cookie_domain` | unset (host-only) | `Domain` on both cookies. Set to `.preview.<domain>` **only** for the wildcard-fleet apex flow, so a session minted at the apex reaches the target subdomain. Safe fleet-wide because the verifier honours the `site` binding (#396) — the cookie travels the fleet, its authority does not. Leave unset for a single host |
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

### One OAuth App for a `*.preview` wildcard fleet

A GitHub OAuth App permits exactly **one** callback host — not a wildcard — so a
`*.preview.<domain>` fleet cannot register one App per preview. Instead, funnel
every callback through a fixed **apex** host and let the module carry the target
preview in the signed OAuth `state`. Mount the issuer on the apex vhost with:

```toml
[middleware.config]
client_id      = "Iv1.…"
client_secret  = "env:GH_CLIENT_SECRET"
session_secret = "env:EPHPM_SESSION_SECRET"   # issuer side: env: is resolved here
redirect_uri   = "https://preview.example.com/_ephpm/auth/github/callback"
cookie_domain  = ".preview.example.com"
sites          = { "pr-1.preview.example.com" = { repo = "acme/web" } }
```

(The verifier on each preview still needs the same key: literally in a
`session-cookie`/`preview-gate` `[[middleware]]` mount, or by `env:`/`file:`
reference in a per-site `[preview_auth]` override.)

Register **`https://preview.example.com/_ephpm/auth/github/callback`** as the
App's Authorization callback URL. Login runs on the target subdomain; the
callback lands on the apex, reads the signed `state`, runs the **target's** authz
check, mints a session whose `site` claim is the **target**, sets it
`Domain=.preview.example.com`, and redirects back to the target. The domain-wide
cookie is safe because the verifier accepts a session only on the preview its
`site` claim names (#396) — so `require_site` must stay on. The full flow, the
open-redirect guard, and the definitive operator values are on the
[Preview Access Gate roadmap page](/roadmap/preview-access-gate/#one-github-oauth-app-for-the-whole-preview-fleet-the-apex-flow).

`github_base` / `github_api_base` must be `https://`, with one exception:
`http://` is accepted for a **loopback** host, which is what lets the test
suite point the module at a stub server. Plaintext to a real GitHub host is
refused outright — it would put the client secret and the user's access token
on the wire in clear.

If `sites` is present it is **authoritative**: a vhost with no entry gets no
access at all, even when a top-level `repo` is also configured. A hostname
nobody mapped must not quietly inherit another tenant's rule.

### Per-preview authorization (`access = "per-preview"`)

A dynamic fleet mints a new `<pr>.preview.<domain>` host per PR, so a static
`sites` map cannot name them and the only fleet-wide `default_check` is coarse
(e.g. "any member of org `acme`"). `access = "per-preview"` authorizes each
preview against **its own PR's base repo** instead. switchboard writes
`repo = "owner/name"` into the preview's `[preview_auth]` override; the router
carries it to this module on a trusted, non-spoofable channel (the middleware
ABI's `request_gate_repo`, never a request header); `start_login` seals it into
the signed OAuth `state` (login always runs on the target host, where the router
knows the repo); and `handle_callback` — which lands on the apex host, where the
router no longer does — rebuilds the `Check::Repo` from the signed state. In this
mode `default_check`/`sites` are optional, and a login or callback with no repo
fails **closed** (403). The session still binds only to the `site` (#396), never
the repo. See the
[Preview Access Gate roadmap page](/roadmap/preview-access-gate/#shipped-per-preview-repository-authorization-issue-487).

### Passive issuer for public-open, private-gated fleets

By default the issuer gates content itself: an unauthenticated request that is
not a login, callback, session, or bypass is redirected to GitHub. That is
correct when every vhost the mount serves is private. On a mixed fleet — some
previews public, some private — it is too much: a **single global issuer holds
the OAuth client secret and must serve login on every subdomain and the callback
on the apex**, so it necessarily runs on public previews too, where redirecting
every visitor to login is wrong.

`endpoints_only = true` makes the issuer **passive**. It still serves its own
endpoints — login starts, the callback is handled, a session cookie hands off to
the verifier, a bypass token still mints — but an unauthenticated **content**
request is passed straight through (`CONTINUE`) instead of being redirected. The
gating decision then belongs entirely to a **per-site verifier**
([`preview-gate`](/guides/native-middleware/#preview-gate), activated per preview
by a `[preview_auth]` override):

- a **private** preview has a verifier, which redirects its unauthenticated
  requests to this issuer's `login_url` — the passive issuer serves that login
  and mints the session;
- a **public** preview has no verifier, so the passive issuer lets its content
  through and the preview is open.

A target is still required in this mode (the login flow runs when someone hits
the login path), so the "no access target" startup error is unchanged. Pair it
with the apex-flow knobs above for a wildcard fleet. The full topology is on the
[Preview Access Gate roadmap page](/roadmap/preview-access-gate/#public-open-private-gated-one-passive-global-issuer).

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
KV surface. (Were it to, that surface is per-site on a multi-tenant node since
ABI minor 3 — issue #376 — see
[KV scope](/guides/native-middleware/#kv-scope-per-tenant-by-default).)

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
the token's `exp` — and, by default, **no `Domain`**, which makes the cookie
host-only. The one exception is `cookie_domain`, which exists for the
[apex flow](#one-oauth-app-for-a-preview-wildcard-fleet): it sets
`Domain=.preview.example.com` on both cookies so a session minted at the apex
reaches the target preview. That does send every session to every preview on
the wildcard; it is safe only because the verifier enforces the `site`
binding (`require_site = true`), so a session travels the fleet but opens only
the preview it was minted for. Leave `cookie_domain` unset on a single host,
and never combine it with `require_site = false`.

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
  URLs. By default `redirect_uri` is derived per request from the vhost, which
  works for one host (or when each preview host is registered). A fleet of
  per-PR hostnames uses the apex hand-off instead — a fixed `redirect_uri`
  plus `cookie_domain`, described in
  [One OAuth App for a `*.preview` wildcard fleet](#one-oauth-app-for-a-preview-wildcard-fleet).
- **No revocation before expiry.** See [The session](#the-session).
- **The `state` cookie is replayable within its 10-minute window** by whoever
  holds it — i.e. by the user themselves. Reuse of an authorization *code* is
  refused by GitHub, not here.
- **The callback is not rate-limited by this module.** Every check that can
  reject a callback locally — the `state` cookie, its signature, its expiry,
  the nonce comparison, the vhost binding, and the shape of `code` — runs
  *before* any network call, so an attacker with no valid `state` cookie
  cannot make this module talk to GitHub at all. A user who has started a
  real login can replay their own callback within its 10-minute window,
  though, and each replay costs one exchange attempt. Mount the `ratelimit`
  builtin on `match = "/_ephpm/auth/*"` if that matters to you.
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
