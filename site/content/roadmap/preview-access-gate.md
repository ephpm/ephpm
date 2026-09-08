# Preview Access Gate

> **Status: PARTIALLY SHIPPED.** The GitHub-identity login gate and its
> per-tenant session verifier have shipped, with the cross-tenant session-replay
> bug (issue #396) fixed. Two further grant paths — a per-site request-phase
> credential (issue #487) and time-limited shareable URLs — are **designed here,
> not yet implemented**. Anything below marked *Planned* is design only.

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

## Planned: per-site request-phase credential (issue #487)

*Design only — not implemented.* The OAuth gate answers "does this GitHub
account have repo access?". Sometimes the operator wants a lighter gate — a
per-preview shared credential — without standing up an OAuth login service, and
switchboard needs a channel to set that credential per preview. The switchboard
side wrote the full design in `ephpm/switchboard:docs/preview-access-gate.md`
(PR #30); this is the ePHPm contract.

### Mechanism

Reuse the same request-phase enforcement point (`static_request_phase` +
`handle_php`), so it covers static and PHP alike and fails closed. Add a **typed
per-site override key** carrying an HTTP Basic-auth **verifier** (never the
plaintext) to the operator-owned per-site override file switchboard already
writes:

```toml
# <site_overrides_dir>/<site-key>.toml   (written by switchboard, not the tenant)
document_root      = "public"
require_basic_auth = "<user>:<bcrypt-or-hmac-of-password>"   # NEW, planned
```

- Add `require_basic_auth` as a typed field on `RawOverride` + `KNOWN_KEYS` in
  `ephpm-server/src/site_overrides.rs`. The loader already validates and
  **fail-closes per site** (a broken override serves nothing for that one site,
  never the wider default — the narrowing-config rule from #463).
- Enforcement: when the resolved site has `require_basic_auth`, a request whose
  `Authorization: Basic` header does not verify (constant-time compare) gets
  `401 WWW-Authenticate: Basic`, ahead of both the static and PHP serving
  paths. The credential is checked against the **operator-owned** override, not
  anything the tenant's PHP can write.
- **Forward-compat:** an ePHPm predating the key treats it as an unknown
  override key and ignores it (the existing per-site-override leniency contract;
  distinct from the strict config-section rule). switchboard only writes the key
  once an enforcing ePHPm is deployed — the same rollout-ordering discipline the
  `document_root` override followed.

### The contract switchboard must satisfy

1. Generate a per-preview credential, hash it, write `require_basic_auth` to
   `<site_overrides_dir>/<site-key>.toml` using the **canonical site key** (the
   `Router::resolve_site` derivation switchboard already ports in
   `src/site_key.rs`), published atomically *before* the site is served.
2. Post the plaintext in the PR comment (the intended audience already has repo
   read access, so seeing it is not a new disclosure — see threat model).
3. Rotate the credential per deploy; remove the override file on teardown.
4. Do **not** put the credential anywhere the tenant's own repo controls — it
   travels the operator-owned override channel only, exactly as `document_root`
   does.

*Alternative considered (option 2 in the switchboard design): expose the site
key to a `RequestCtx` and add a builtin `preview_auth` module reading a per-site
source. More surface than preview-privacy warrants; the override-key path is
smaller and reuses machinery that already fail-closes per site.*

## Planned: temporary shareable URLs

*Design only — not implemented.* A second grant path into the **same**
enforcement point, for people who lack GitHub repo access — stakeholders,
designers, a client — so they can see a preview without a login and without
being added to the repo.

### Shape

A time-limited, signed **capability token**, minted with the same HS256 secret
the `session-cookie` gate already holds, carrying:

- `site` = the preview's canonical site key (so it is **per-preview**, checked
  by the exact `require_site` binding above — a share link for preview A never
  opens B);
- `via = "share"` (distinguishes it from an OAuth session in logs/audit);
- a short `exp` (default **≤ 1 hour**, hard-capped well below a session TTL);
- optionally `jti` (see revocation).

The gate accepts it as an **alternative** to the OAuth session at the same
verification point: presented either as the session cookie (a share link that
lands and sets the cookie) or as a `?ephpm_share=<token>` query parameter that
the gate exchanges for the cookie on first use and strips from the redirect.
Because it verifies through the same `Hs256Policy` with the same `expected_site`,
**no new verification code and no second verifier** — the property #396's
one-verifier design exists to protect.

### Minting

Two options, both keeping the OAuth flow the only thing that talks to GitHub:

1. **By switchboard** (operator control plane), on request from an authenticated
   repo member — switchboard already holds the secret to write per-site config,
   so it can mint. Simplest; no new ePHPm endpoint.
2. **By an authenticated repo-member hitting an ePHPm endpoint** — a reserved
   path on `github-auth` (e.g. `/_ephpm/auth/github/share`) that, *only* for a
   request already carrying a valid OAuth session for this preview, mints a
   `via:"share"` token with a bounded TTL and returns the link. This keeps
   minting gated on real repo access and needs no switchboard round trip.

Issuance is deliberately unprivileged in the sense that *anything holding the
secret* can mint — that is inherent to a self-contained HS256 token and is why
the blast radius is bounded below, not by an issuance ACL.

### Expiry and revocation

- **Expiry** is the primary control: a short `exp` means a leaked link stops
  working on its own. This is the same "TTL is the blast radius" property as the
  OAuth session, tightened.
- **Revocation before expiry** needs state, since the token is self-contained.
  The design uses the embedded KV store the gate can already reach: a
  **deny-list** keyed by `jti` (`share:revoked:<jti>` with a TTL == the token's
  remaining life), replicated across the cluster by gossip, checked on the hot
  path only for `via:"share"` tokens (so a normal session pays nothing). Teardown
  of a preview revokes all its outstanding share tokens by writing the site's
  epoch marker (`share:epoch:<site>`) — a token minted before the current epoch
  is refused, so closing a PR invalidates every share link for it at once
  without enumerating `jti`s.

### Threat model — say it plainly

A share URL is a **bearer capability**: anyone who has the link is in, until it
expires or is revoked. That is the point (sharing with people who cannot
authenticate), and it is a *weaker* property than the OAuth gate — state it in
every place a link is minted. Blast radius is bounded by design:

- **Per-preview** (`site` claim + `require_site`): a link opens exactly one
  preview, never the fleet.
- **Short-lived** (`exp` ≤ 1 hour default): a leaked link self-heals.
- **Revocable** (KV deny-list + per-site epoch): a link can be killed before
  expiry, and teardown kills all of them.
- **HTTPS-only and same transport rules** as the session it stands in for.

What it explicitly does **not** defend: someone the link was shared with
forwarding it within its TTL, or a compromised holder. Those are accepted for a
preview-privacy feature and out of scope; a preview is not a secrets vault.

## Relationship to multi-tenant isolation

This gate defends **preview privacy** — stopping an internet visitor who guessed
a hostname from reading a preview. It is **not** the multi-tenant isolation
boundary (per-site databases, KV keyspaces, `open_basedir`), which is a separate,
existing property. The one place the two meet is the canonical site key: the gate
binds to the exact identity the isolation layer uses, so "authorized for preview
A" and "served preview A's data" are the same tenant by construction.
