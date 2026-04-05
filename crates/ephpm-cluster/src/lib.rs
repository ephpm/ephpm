//! Gossip-based clustering for ePHPm.
//!
//! Uses the [`chitchat`] crate (SWIM protocol) for decentralized peer
//! discovery and phi-accrual failure detection. Exposes a
//! [`ClusterHandle`] for querying live cluster membership.
//!
//! # Usage
//!
//! ```toml
//! [cluster]
//! enabled = true
//! bind = "0.0.0.0:7946"
//! join = ["10.0.1.2:7946"]
//! ```

pub mod clustered_store;
pub mod gossip_kv;
pub mod kv_data_plane;
pub mod node;
pub mod sqlite_election;

pub use clustered_store::ClusteredStore;
pub use kv_data_plane as data_plane;
pub use node::{ClusterHandle, NodeInfo, NodeState, start_gossip};
pub use sqlite_election::{ElectedRole, SqliteElection};
