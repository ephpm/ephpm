//! E2E test helpers for ephpm.
//!
//! This crate is excluded from the workspace and only built inside the
//! E2E test runner container (see `docker/Dockerfile.e2e`).

/// Read an environment variable or panic with a helpful message.
pub fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} environment variable must be set"))
}
