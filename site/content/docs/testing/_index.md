+++
title = "Testing"
weight = 8
+++

How ePHPm is tested. The strategy is layered: in-process unit tests catch logic bugs, integration tests exercise full HTTP/PHP/SQL paths in-process, and end-to-end tests in Kubernetes verify clustering and replication.

- **[Strategy](strategy/)** — the overall testing philosophy and what each tier covers.
- **[Unit](unit/)** — in-crate tests with `cargo test -p <crate>`.
- **[End-to-End](e2e/)** — Kind cluster + Tilt via `cargo xtask e2e`.
- **[Nightly](nightly/)** — long-running cluster, fault injection, soak tests.
