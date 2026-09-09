# `ephpm-middleware-github-auth`

A GitHub OAuth **login gate** for ePHPm, shipped as a `dlopen`'d native
middleware module. It answers *"does the person at this browser have access,
on GitHub, to the repo this preview is for?"* — **once**, at login — and then
issues a stateless signed session.

Full operator documentation: **[GitHub OAuth Gate
(native middleware)](../../site/content/guides/github-auth-middleware.md)**.

```bash
cargo build --release -p ephpm-middleware-github-auth
# target/release/libgithub_auth.so | .dylib | github_auth.dll
```

```toml
[[middleware]]
library = "/opt/ephpm/modules/libgithub_auth.so"
order   = 10
config  = { client_id = "Iv1.…", client_secret = "env:GH_CLIENT_SECRET",
            session_secret = "env:EPHPM_SESSION_SECRET", repo = "acme/web" }
```

## The OAuth App callback URL

The module's default endpoints live under `/_ephpm/auth/`:

- **Login:** `/_ephpm/auth/github/login`
- **Callback:** `/_ephpm/auth/github/callback`

The ePHPm router routes the reserved `/_ephpm/auth/` sub-namespace **to the
middleware chain** (the rest of `/_ephpm/` is server-internal), so these are
reachable without leaving the reserved namespace or colliding with an app
route. Register this as the GitHub OAuth App's **Authorization callback URL**:

```
https://<host>/_ephpm/auth/github/callback
```

Keep `login_path`/`callback_path` under `/_ephpm/auth/` — a non-`/_ephpm/` value
leaves the carve-out and would need to avoid the app's own routes.

### One App across a `*.preview` wildcard fleet (the apex flow)

A GitHub OAuth App allows exactly **one** callback host — **not** a wildcard. So
a preview fleet does **not** register `*.preview.<domain>`; it funnels every
callback through one fixed **apex** host and carries the target preview in the
signed OAuth `state`. The issuer is a single global `[[middleware]]` mount (it
runs on every vhost — login starts on the target subdomain, the callback lands
on the apex, same mount). Configure it with:

```toml
config = { client_id = "Iv1.…", client_secret = "env:GH_CLIENT_SECRET",
           session_secret = "env:EPHPM_SESSION_SECRET",
           # apex flow: one fixed callback + a fleet-wide cookie
           redirect_uri  = "https://preview.example.com/_ephpm/auth/github/callback",
           cookie_domain = ".preview.example.com",
           sites = { "pr-1.preview.example.com" = { repo = "acme/web" } } }
```

Register **`https://preview.example.com/_ephpm/auth/github/callback`** (the apex)
as the App's Authorization callback URL. Login runs on the target subdomain; the
callback lands on the apex, reads the signed `state`, runs the **target's** authz
check, mints a session whose `site` claim is the **target**, sets it with
`Domain=.preview.example.com`, and `302`s back to the target. A domain-wide
cookie is safe here only because the session verifier honours the `site` binding
(#396): the cookie travels the fleet, but verifies on exactly one preview. See
`cookie.rs` for the full argument.

## Three things to know before reading the code

1. **This is the cold path only.** It issues sessions; it never verifies one.
   A request carrying the session cookie gets `CONTINUE` and a separate
   session-verifier module decides. **Mounted alone it is not an
   authenticator** — it logs that at startup.
2. **No GitHub call ever happens on a request that has a session.** That is
   structural: `GithubAuth::route` is a pure function, only its `Callback`
   variant reaches `github.rs`, and only the exact configured `callback_path`
   produces it. `tests/oauth_round_trip.rs` asserts the stub GitHub's request
   counter does not move across 50 authenticated requests.
3. **It is a `cdylib`, not a builtin, on purpose.** It needs an HTTP client
   and TLS; none of that is in the `ephpm` binary
   (`cargo tree -e features,no-dev -p ephpm -i ring` is byte-identical with
   and without this crate). The module itself links exactly one crypto
   provider, `aws-lc-rs` — see the comment on the `rustls` dependency in
   `Cargo.toml`, which is load-bearing.

## Layout

| file | what it holds |
|---|---|
| `src/lib.rs` | routing (`Route`), the login/callback/bypass flows, `declare!` |
| `src/config.rs` | strict, fail-closed config parsing; `env:` secret indirection |
| `src/github.rs` | the outbound half: TLS, token exchange, the three access checks |
| `src/token.rs` | HS256 minting, key derivation, constant-time comparison |
| `src/redirect.rs` | return-to validation (the open-redirect defence) |
| `src/cookie.rs` | cookie reading and `Set-Cookie` construction |
| `tests/oauth_round_trip.rs` | the whole flow against a stub GitHub over real sockets |

## Tests

```bash
cargo test -p ephpm-middleware-github-auth
```

What is **not** covered, and needs a human: a round trip against the real
`github.com` with a registered GitHub App. Everything this side of that is
exercised over real TCP against a stub.
