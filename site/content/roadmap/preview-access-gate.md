# Preview Access Gate

> **Status: SHIPPED (enforcement + verification); minting is switchboard's.**
> The GitHub-identity login gate and its per-tenant session verifier shipped
> with the cross-tenant session-replay fix (issue #396). **Stage 2 (this page)
> adds the request-phase *enforcement*:** per-site activation of the gate via
> the `[preview_auth]` override key (issue #487) and time-limited, revocable
> shareable-URL capability tokens — both enforced on the static **and** PHP
> paths, fail-closed. What ePHPm does **not** do is *mint* credentials: the
> OAuth session is minted by the `github-auth` issuer, and share links are
> minted by the control plane (switchboard) — see the contract below. Sections
> still marked *Planned* are design only.

A deployed preview at `<label>.preview.ephpm.dev` resolves for anyone who knows
the hostname. Fetching **private** code for a preview (switchboard#26) is
pointless if the resulting site is world-readable, so a preview needs an access
gate. This page collects the three grant paths into one enforcement story.

## What ships now: the GitHub OAuth login gate (issues #388/#389, #396 fixed)

Two composed native-middleware modules, documented in full at
[GitHub OAuth gate](/guides/github-auth-middleware/) and
[`session-cookie`](/guides/native-middleware/#session-cookie):

- **`github-auth`** (dlopen cdylib, cold path) runs the OAuth round trip against
  GitHub *once* at login — "does this browser's GitHub account have read access
  to the repo this preview is for?" — and mints a stateless HS256 session bound
  to the preview via a `site` claim.
- **`session-cookie`** (builtin, hot path) verifies that session on every
  request with a single local HMAC — no network call — and redirects
  unauthenticated browsers to the login path.

### The #396 fix — per-tenant binding

The issuer always bound each session to one preview (`"site": <vhost>` claim),
but the verifier originally **ignored** that claim: a session minted for preview
A verified on every preview on the node — a cross-tenant auth bypass on the one
deployment shape the feature exists for.

The fix makes the verifier honour the binding, and makes issuer and verifier
agree on identity **by construction**:

- Both derive the tenant identity from the router's one canonical site key
  (`Request::vhost_id()`, ABI minor 3 / issue #390) — never from the raw `Host`.
- `Hs256Policy::verify(token, now, expected_site: Option<&str>)` gained the
  `expected_site` parameter. When `Some`, the token's `site` claim must be a
  string equal to it; absent, non-string, or mismatched fails closed. The `jwt`
  API gate passes `None` (it has no tenancy).
- `session-cookie` gained `require_site` (**default `true`**). It passes the
  request's canonical site key as `expected_site`. If the request matched no
  vhost (unrecognised `Host`, or a single-site node), it cannot bind a session
  to a tenant and refuses with a hard `403`. `require_site = false` is an
  explicit single-tenant opt-out, never a preview-host setting.

The regression guard is a test that mints a session for `site:"alpha"` and
asserts it is **rejected** when serving `beta` and accepted only on `alpha`
(`ephpm-middleware-builtins`, `session_cookie::tests`), plus the crypto-core
proof that the pre-fix call shape (`verify(.., None)`) still accepts the token
while the site-aware call rejects it off its site.

### Static assets are already covered

Because the middleware **request phase** runs on both the PHP path and
`Router::static_request_phase` (issue #395) — with the same canonical site key
and connection scheme on both — mounting `session-cookie` gates a preview's
**static files too**, per-site and fail-closed, with no extra work. An
unauthenticated `GET /wp-content/uploads/secret.png` redirects to login exactly
as `GET /index.php` does. This is the property a switchboard
`auto_prepend_file` gate could never have: that runs only on the PHP path.

## Shipped: per-site activation via the `[preview_auth]` override (issue #487)

The gate turns on for one preview through a **typed section in the
operator-owned per-site override file** switchboard already writes for
`document_root`/`auto_prepend_file`. When a resolved site has a valid
`[preview_auth]` section, ePHPm builds a per-site
[`preview-gate`](/guides/native-middleware/#preview-gate) instance and runs it
in the request phase on **both** `static_request_phase` and `handle_php`, ahead
of serving — so an unauthenticated request to a gated preview is redirected to
login (or `403`), and the content — a script *or* a static file — is never
served.

```toml
# <site_overrides_dir>/<site-key>.toml   (written by switchboard, not the tenant)
document_root = "public"

[preview_auth]                                    # turns the gate ON for this vhost
session_secret = "env:EPHPM_PREVIEW_SESSION_SECRET"   # HS256 key, shared with the issuer
login_url      = "/_ephpm/auth/github/login"          # the issuer's login endpoint
# cookie / issuer / audience / require_https / require_site / share_* are optional.
# exempt_paths is NOT needed for the default `/_ephpm/auth/…` endpoints — the
# router carve-out means the gate never sees them.
```

### Why activation lives here, and what stays out

**Why the override, not `[[middleware]]`.** A preview fleet mints a new vhost
per PR, and ePHPm has no runtime config reload — the override file, re-read
every `SITE_CONFIG_TTL` (~2 s), is switchboard's *only* per-preview channel. A
global `session-cookie`/`github-auth` mount in `ephpm.toml` cannot be turned on
for a brand-new preview without a restart; the override can.

**What is deliberately NOT in the override.** Only the *enforcement* half lives
here. The OAuth **issuer** (`github-auth` — the cold-path login/callback round
trip that holds the GitHub App's `client_id`/`client_secret` and the per-repo
access check) stays a normal operator-owned `[[middleware]]` mount. Putting an
OAuth **client secret** into this file would be a security regression: the file
is derived from a manifest inside the tenant's own repository, so every value in
it is transitively tenant-influenced — the same argument
[`site_overrides`](/guides/virtual-hosts/) uses to refuse an arbitrary `ini`
table. So the override carries the *session secret* (typically an `env:`/`file:`
**reference** both the issuer and the gate resolve — one source of truth, secret
never in the file or the served tree) and the *login entry point*, and the two
halves are coupled by that shared secret and a matching `cookie` name plus the
`site`-claim binding (#396).

**Secret resolution.** `session_secret` accepts `env:NAME` (read that variable),
`file:/abs/path` (read the file, trimmed), or a literal (discouraged). It must
resolve to at least 32 bytes — the same floor the issuer enforces on the key it
signs with.

### Fail-closed is the whole point

`[preview_auth]` is a **narrowing** instruction, exactly like `document_root`: a
section ePHPm understood but could not turn into a working gate (missing/short
secret, unresolvable `env:`, unreadable `file:`, missing `login_url`) takes the
one preview **out of service (503)** rather than serving it ungated. A daemon
interrupted mid-write, or a typo, can never leave a preview open to the
internet. (Unknown *keys* inside the section stay lenient and are reported,
matching the forward-compat rule the override file already follows for unknown
top-level keys — a newer switchboard can add a key without taking a fleet
down.)

### The OAuth endpoints live under `/_ephpm/auth/` (reachable)

The reserved `/_ephpm/` namespace is answered before the middleware chain — but
the router **carves out `/_ephpm/auth/`** and routes it *to* the request-phase
chain (`AUTH_NAMESPACE_PREFIX` / `Router::handle_auth_namespace`), specifically
so a mounted `github-auth` issuer can serve its login and callback there. So the
issuer's defaults work unchanged:

- **Login:** `https://<preview-host>/_ephpm/auth/github/login`
- **Callback (the URL to register in the GitHub OAuth App):**
  `https://<preview-host>/_ephpm/auth/github/callback`

Everything else under `/_ephpm/` (and the bare `/_ephpm`) still 404s, and the
carve-out never reaches the application: with no auth module mounted a
`/_ephpm/auth/…` request is a `404`, never the PHP/worker catch-all. The
per-site gate is **not** run on `/_ephpm/auth/` (the issuer's own endpoints must
answer an unauthenticated visitor), so no `exempt_paths` entry is needed for the
default paths. Keep the issuer's `login_path`/`callback_path` under
`/_ephpm/auth/` — a non-`/_ephpm/` path would leave the reserved namespace and
risk colliding with an app route.

## Shipped: temporary shareable URLs (verification + revocation)

A second grant path into the **same** enforcement point, for people who lack
GitHub repo access — a stakeholder, a designer, a client — so they can see one
preview without a login and without being added to the repo. ePHPm implements
the **verification and revocation** side; *minting* is the control plane's job
(see the contract).

### Shape

A time-limited, signed **capability token**, minted with the same HS256 secret
the gate already holds, carrying:

- `site` = the preview's canonical site key (so it is **per-preview**, checked
  by the exact `site`-claim binding the OAuth session uses — a share link for
  preview A never opens B);
- `via = "share"` (distinguishes it from an OAuth session, and is what gates the
  extra revocation checks);
- `exp` (the minter keeps it short — the verifier enforces only that `exp`
  exists and is in the future);
- `iat` (issue time — the epoch revocation compares against it);
- `jti` (a unique id, so an individual link can be revoked).

The [`preview-gate`](/guides/native-middleware/#preview-gate) accepts it as an
**alternative** to the OAuth session at the same verification point: presented
either as the session cookie, or as a `?ephpm_share=<token>` query parameter the
gate verifies, plants as the session cookie (`Set-Cookie`, `HttpOnly`,
`Max-Age` = the token's remaining life), and strips from a `303` redirect to the
clean URL. Because it verifies through the same `Hs256Policy` with the same
`expected_site`, there is **no new verification code and no second verifier** —
the property #396's one-verifier design exists to protect. A share token
presented as a cookie is subject to the same revocation checks as one in the
query.

### Minting — the control plane's job

Issuance is deliberately unprivileged in the cryptographic sense: *anything
holding the HS256 secret* can mint, which is inherent to a self-contained token
and is why the blast radius is bounded by the short `exp` and by revocation, not
by an issuance ACL. ePHPm ships the **reference minter**
(`ephpm_middleware_builtins::preview_gate::mint_share_token`, used by the tests
and mirrored by the control plane); it does not expose a mint endpoint. The
control plane (switchboard) mints on request from an authenticated repo member —
it already holds the secret to write per-site config — and posts the link in the
PR comment. (A future ePHPm endpoint on `github-auth` that mints only for a
request already carrying a valid OAuth session is possible, but is not required
and is not in this stage.)

### Expiry and revocation — implemented

- **Expiry** is the primary control: a short `exp` means a leaked link stops
  working on its own. Same "TTL is the blast radius" property as the OAuth
  session, tightened.
- **Individual revocation** uses the embedded KV store the gate reaches through
  the host table, in the request's **own per-vhost keyspace**: a deny-list keyed
  by `jti` (`preview:share:revoked:<jti>`), checked on the hot path **only** for
  `via:"share"` tokens — a normal session pays nothing. When clustered, the
  per-vhost KV is gossip-replicated, so a revoke propagates across nodes.
- **Revoke-all (teardown)** writes a per-site **epoch**: `preview:share:epoch`
  in the site's KV keyspace (or the static `share_epoch` config floor). A share
  token whose `iat` is below the effective epoch is refused, so closing a PR —
  or a rotation — invalidates every outstanding share link for that preview at
  once, without enumerating `jti`s. (A token with no `iat` is treated as `0`, so
  any non-zero epoch refuses it — fail-closed.)

### Threat model — say it plainly

A share URL is a **bearer capability**: anyone who has the link is in, until it
expires or is revoked. That is the point (sharing with people who cannot
authenticate), and it is a *weaker* property than the OAuth gate — state it in
every place a link is minted. Blast radius is bounded by design:

- **Per-preview** (`site` claim + `require_site`): a link opens exactly one
  preview, never the fleet — enforced by ePHPm.
- **Short-lived** (`exp`): the minter keeps it small; ePHPm enforces that `exp`
  exists and is in the future, so a leaked link self-heals.
- **Revocable** (per-`jti` KV deny-list + per-site epoch): a link can be killed
  before expiry, and teardown kills all of them at once — enforced by ePHPm.
- **Same transport rules** as the session it stands in for (`require_https`
  defaults on; loopback exempt for local development).

What it explicitly does **not** defend: someone the link was shared with
forwarding it within its TTL, or a compromised holder. Those are accepted for a
preview-privacy feature and out of scope; a preview is not a secrets vault.

## The switchboard contract

ePHPm implements activation, enforcement and verification. switchboard (a
separate repo) implements the control plane. This is exactly what it must do —
build against this, not against the ePHPm internals.

**One-time, per fleet (operator config, not per preview):**

1. Register one GitHub App and mount the `github-auth` **issuer** globally in
   `ephpm.toml` (`[[middleware]] library = "github-auth"`), with its
   `client_id`/`client_secret`, the per-repo/org access target (its own `sites`
   map or `default_check`), and `session_secret = "env:EPHPM_PREVIEW_SESSION_SECRET"`.
   Keep its `login_path`/`callback_path` at the defaults under `/_ephpm/auth/`
   (the router routes that sub-namespace to the chain). The GitHub OAuth App's
   **Authorization callback URL** is then
   `https://<preview-host>/_ephpm/auth/github/callback` — for a wildcard preview
   fleet, register `https://*.preview.<domain>/_ephpm/auth/github/callback` (or
   the specific hosts your App allows).
2. Set `EPHPM_PREVIEW_SESSION_SECRET` in the ePHPm process environment (≥ 32
   bytes). This is the one source of truth for the HS256 key; the issuer and
   every preview's gate reference it, never a literal.

**Per preview, at deploy (atomically, before the site is served):**

3. Write `[preview_auth]` into `<site_overrides_dir>/<site-key>.toml`, using the
   **canonical site key** (the `Router::resolve_site` derivation switchboard
   already ports in `src/site_key.rs`) as the filename:
   ```toml
   [preview_auth]
   session_secret = "env:EPHPM_PREVIEW_SESSION_SECRET"   # the SAME reference the issuer uses
   login_url      = "/_ephpm/auth/github/login"          # the issuer's login endpoint
   ```
   `cookie` must match the issuer's `cookie_name` (both default `ephpm_session`,
   so usually omit it). No `exempt_paths` is needed for the default
   `/_ephpm/auth/…` endpoints.
4. **Rollout ordering:** only write `[preview_auth]` once an ePHPm that enforces
   it is deployed. An older ePHPm treats the unknown section leniently (ignored,
   reported) and would serve the preview **ungated** — so a fleet upgrades ePHPm
   first, then starts writing the key. Same discipline `document_root` followed.

**Share links (optional, per request from a repo member):**

5. Mint a `via:"share"` token with the fleet secret, a short `exp`, a fresh
   random `jti`, and `iat = now`, `site = <canonical site key>` — mirror
   `ephpm_middleware_builtins::preview_gate::mint_share_token`. Hand it out as
   `https://<preview-host><path>?ephpm_share=<token>` and post it in the PR
   comment with the bearer-capability warning.
6. **Revoke one link:** write `preview:share:revoked:<jti>` (any value, TTL = the
   token's remaining life) into that preview's KV keyspace.
7. **Revoke all links / teardown:** write `preview:share:epoch` = `now` (unix
   seconds) into that preview's KV keyspace; every share token issued before
   that instant is refused. On PR close, also remove `<site-key>.toml` (which
   removes the gate) and the checkout.

**What switchboard must NOT do:** put the OAuth `client_secret` — or any GitHub
App credential — into a `<site-key>.toml`. That file is derived from
tenant-controlled repository content; secrets travel the operator-owned global
mount and the process environment only.

## Relationship to multi-tenant isolation

This gate defends **preview privacy** — stopping an internet visitor who guessed
a hostname from reading a preview. It is **not** the multi-tenant isolation
boundary (per-site databases, KV keyspaces, `open_basedir`), which is a separate,
existing property. The one place the two meet is the canonical site key: the gate
binds to the exact identity the isolation layer uses, so "authorized for preview
A" and "served preview A's data" are the same tenant by construction.
