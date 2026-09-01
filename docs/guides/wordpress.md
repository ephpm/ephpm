# Running WordPress on ePHPm

**This guide has moved.** The canonical, maintained version lives on the docs
site:

> **https://ephpm.dev/guides/wordpress/**
> (source: [`site/content/guides/wordpress.md`](../../site/content/guides/wordpress.md))

This file used to be a near-duplicate of that page, and it drifted: it pinned
container images two releases behind, showed a `0.7.0` startup banner, told you
to `composer require ephpm/cache-wordpress` without the `vcs` `repositories`
entry that a non-Packagist package needs, and verified the object cache with an
`ephpm kv keys "*"` command that cannot work under `ephpm dev` (the CLI reaches
the store over the RESP listener, which is off by default and cannot be turned
on for a dev server). Keeping two copies in sync is what produced that drift,
so there is now one copy.

The site guide covers the same three paths — `ephpm dev`, Docker, and
Kubernetes — plus the SQLite drop-in, the `ephpm/cache-wordpress` object cache,
and troubleshooting.

Related:

- [WordPress worker mode](https://ephpm.dev/guides/wordpress-worker/) — boot
  WordPress once per thread instead of per request (experimental)
- [Virtual hosts](https://ephpm.dev/guides/virtual-hosts/) — many WordPress
  sites on one ePHPm process
- [`examples/wordpress-compose/`](../../examples/wordpress-compose/) — a
  runnable Docker Compose setup using the DB proxy against a real MySQL
