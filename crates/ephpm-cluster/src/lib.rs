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

pub mod cluster_channel;
pub mod clustered_store;
pub mod gossip_kv;
pub mod gossip_writer;
pub mod kv_data_plane;
pub mod node;
pub mod reconnect;
pub mod secure_transport;
pub mod site_namespace;
pub mod sqlite_election;

pub use cluster_channel::{
    ChannelStream, FeatureFlags as ChannelFeatureFlags, Handle as ChannelHandle, IncomingStream,
    maybe_start as maybe_start_cluster_channel, peer_is_cluster_member, stream_type,
};
pub use clustered_store::{ClusteredStore, KvReplicator, SiteKvReplicator};
pub use gossip_kv::{Stamp, StampClock, node_tag};
pub use kv_data_plane as data_plane;
pub use node::{ClusterHandle, NodeInfo, NodeState, start_gossip};
pub use reconnect::ReconnectBackoff;
pub use secure_transport::{ClusterCipher, EncryptedUdpTransport};
pub use sqlite_election::{
    ElectedRole, HRW_SCHEME, HrwSchemeDebounce, SqliteElection, describe_hrw_scheme_mismatches,
    hrw_owner, hrw_scheme_mismatches, per_site_primary, spawn_hrw_scheme_watch,
};
