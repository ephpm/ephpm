//! Integration tests for gossip cluster formation.
//!
//! Spawns lightweight cluster nodes on localhost with random UDP ports and
//! verifies peer discovery, KV propagation, and node departure detection.

use std::net::SocketAddr;
use std::time::Duration;

use ephpm_cluster::{start_gossip, NodeState};
use ephpm_config::ClusterConfig;

/// Find a free UDP port by binding to port 0 and returning the address.
///
/// The port is released before returning, so there is a small race window,
/// but for localhost tests this is reliable enough.
fn free_udp_addr() -> SocketAddr {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind to port 0");
    sock.local_addr().expect("local addr")
}

/// Helper: create a cluster config bound to a specific address on localhost.
fn node_config(node_id: &str, cluster_id: &str, bind: SocketAddr, seeds: Vec<String>) -> ClusterConfig {
    ClusterConfig {
        enabled: true,
        bind: bind.to_string(),
        join: seeds,
        secret: String::new(),
        node_id: node_id.to_string(),
        cluster_id: cluster_id.to_string(),
        ..ClusterConfig::default()
    }
}

#[tokio::test]
async fn single_node_sees_itself() {
    let addr = free_udp_addr();
    let config = node_config("node-1", "test-cluster-single", addr, vec![]);
    let handle = start_gossip(&config).await.expect("gossip should start");

    let nodes = handle.nodes().await;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, "node-1");
    assert_eq!(nodes[0].state, NodeState::Alive);

    let self_node = handle.self_node();
    assert_eq!(self_node.id, "node-1");

    assert_eq!(handle.cluster_id(), "test-cluster-single");
    assert_eq!(handle.live_node_count().await, 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn two_nodes_discover_each_other() {
    let addr1 = free_udp_addr();
    let addr2 = free_udp_addr();
    let seed = addr1.to_string();

    let config1 = node_config("node-a", "test-cluster-2node", addr1, vec![]);
    let handle1 = start_gossip(&config1).await.expect("node-a should start");

    let config2 = node_config("node-b", "test-cluster-2node", addr2, vec![seed]);
    let handle2 = start_gossip(&config2).await.expect("node-b should start");

    // Wait for gossip convergence (SWIM protocol needs a few rounds).
    let mut discovered = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let count = handle1.live_node_count().await;
        if count >= 2 {
            discovered = true;
            break;
        }
    }

    assert!(discovered, "node-a should discover node-b within 4 seconds");

    // Verify node-b also sees node-a.
    let nodes_from_b = handle2.nodes().await;
    let alive_from_b: Vec<_> = nodes_from_b
        .iter()
        .filter(|n| n.state == NodeState::Alive)
        .collect();
    assert!(
        alive_from_b.len() >= 2,
        "node-b should see at least 2 alive nodes, got {}",
        alive_from_b.len()
    );

    handle1.shutdown().await;
    handle2.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_propagates_between_nodes() {
    let addr1 = free_udp_addr();
    let addr2 = free_udp_addr();
    let seed = addr1.to_string();

    let config1 = node_config("kv-node-1", "test-kv-cluster", addr1, vec![]);
    let handle1 = start_gossip(&config1).await.unwrap();

    let config2 = node_config("kv-node-2", "test-kv-cluster", addr2, vec![seed]);
    let handle2 = start_gossip(&config2).await.unwrap();

    // Wait for peer discovery.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if handle1.live_node_count().await >= 2 {
            break;
        }
    }

    // Set a KV entry on node-1.
    handle1
        .gossip_set("test-key", b"test-value", None)
        .await;

    // Wait for it to propagate to node-2.
    let mut found = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(value) = handle2.gossip_get("test-key").await {
            assert_eq!(value, b"test-value");
            found = true;
            break;
        }
    }

    assert!(found, "KV entry should propagate from node-1 to node-2 within 4 seconds");

    // Verify gossip_exists.
    assert!(handle2.gossip_exists("test-key").await);
    assert!(!handle2.gossip_exists("nonexistent-key").await);

    // Verify gossip_keys includes the key.
    let keys = handle2.gossip_keys().await;
    assert!(keys.contains(&"test-key".to_string()));

    handle1.shutdown().await;
    handle2.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_with_ttl() {
    let addr = free_udp_addr();
    let config = node_config("ttl-node", "test-ttl-cluster", addr, vec![]);
    let handle = start_gossip(&config).await.unwrap();

    // Set with a long TTL.
    handle
        .gossip_set("ttl-key", b"data", Some(Duration::from_secs(3600)))
        .await;

    let value = handle.gossip_get("ttl-key").await;
    assert!(value.is_some(), "key with future TTL should be readable");

    let pttl = handle.gossip_pttl("ttl-key").await;
    assert!(pttl.is_some(), "key should have a TTL");
    let ttl_ms = pttl.unwrap();
    assert!(ttl_ms > 3_500_000, "TTL should be roughly 3600s in ms");

    handle.shutdown().await;
}

#[tokio::test]
async fn gossip_del_removes_key() {
    let addr = free_udp_addr();
    let config = node_config("del-node", "test-del-cluster", addr, vec![]);
    let handle = start_gossip(&config).await.unwrap();

    handle.gossip_set("del-me", b"value", None).await;
    assert!(handle.gossip_exists("del-me").await);

    let deleted = handle.gossip_del("del-me").await;
    assert!(deleted, "should return true for existing key");

    // After delete, key should no longer be found.
    let value = handle.gossip_get("del-me").await;
    assert!(value.is_none(), "deleted key should not be readable");

    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "failure detection timing depends on chitchat's phi-accrual detector, may take 15-30s"]
async fn node_departure_detected() {
    let addr1 = free_udp_addr();
    let addr2 = free_udp_addr();
    let seed = addr1.to_string();

    let config1 = node_config("survivor", "test-departure", addr1, vec![]);
    let handle1 = start_gossip(&config1).await.unwrap();

    let config2 = node_config("departing", "test-departure", addr2, vec![seed]);
    let handle2 = start_gossip(&config2).await.unwrap();

    // Wait for both nodes to see each other.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if handle1.live_node_count().await >= 2 {
            break;
        }
    }
    assert!(
        handle1.live_node_count().await >= 2,
        "should have 2 live nodes before departure"
    );

    // Shut down the departing node.
    handle2.shutdown().await;

    // Wait for the failure detector to notice. chitchat's default failure
    // detector may take several seconds with default intervals.
    let mut departed = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let count = handle1.live_node_count().await;
        if count <= 1 {
            departed = true;
            break;
        }
    }

    assert!(departed, "survivor should detect departing node within 10 seconds");

    handle1.shutdown().await;
}
