//! Embedded KV store with RESP protocol for ePHPm.
//!
//! Provides an in-memory key-value store that speaks the Redis RESP2 protocol,
//! allowing existing Redis clients (like PHP's `phpredis` or `predis`) to
//! connect without any code changes.
//!
//! # Architecture
//!
//! - **[`store`]**: `DashMap`-backed concurrent store with TTL, LRU eviction,
//!   and approximate memory tracking.
//! - **[`expiry`]**: the background active-expiry thread that reclaims expired
//!   keys in the default store and every per-site store.
//! - **[`resp`]**: RESP2 wire protocol parser and serializer.
//! - **[`command`]**: Redis command dispatch — translates RESP frames into
//!   store operations.
//! - **[`server`]**: Tokio TCP server accepting RESP connections.
//! - **[`snapshot`]**: encrypted warm-restart snapshots — write the store to
//!   a file at shutdown, load it at the next start.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use ephpm_kv::store::{Store, StoreConfig};
//! use ephpm_kv::server::{self, ServerConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let store = Store::new(StoreConfig::default());
//! server::run(store, ServerConfig::default(), None).await?;
//! # Ok(())
//! # }
//! ```

pub mod auth;
pub mod command;
pub mod expiry;
pub mod multi_tenant;
pub mod resp;
pub mod server;
pub mod snapshot;
pub mod store;
