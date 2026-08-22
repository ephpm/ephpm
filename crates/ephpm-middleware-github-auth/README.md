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
