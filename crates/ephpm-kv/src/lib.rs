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
//! - **[`resp`]**: RESP2 wire protocol parser and serializer.
//! - **[`command`]**: Redis command dispatch — translates RESP frames into
//!   store operations.
//! - **[`server`]**: Tokio TCP server accepting RESP connections.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use ephpm_kv::store::{Store, StoreConfig};
//! use ephpm_kv::server::{self, ServerConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let store = Store::new(StoreConfig::default());
//! server::run(store, ServerConfig::default()).await?;
//! # Ok(())
//! # }
//! ```

pub mod command;
pub mod resp;
pub mod server;
pub mod store;
