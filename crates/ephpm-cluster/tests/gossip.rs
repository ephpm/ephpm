//! Gossip clustering integration tests.
//!
//! Boots multiple ephpm-cluster instances on random UDP ports and verifies
//! they discover each other via the SWIM gossip protocol.

use std::time::{Duration, Instant};

use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_config::{ClusterConfig, ClusterKvConfig};

/// Allocate a random UDP port by binding to :0 and immediately closing.
async fn random_udp_port() -> u16 {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("failed to bind UDP socket for port allocation");
    sock.local_addr().expect("failed to get local addr").port()
}

/// Start a gossip node on the given port with the given seeds.
async fn start_node(port: u16, seeds: Vec<String>, node_id: &str) -> ClusterHandle {
    let config = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{port}"),
        join: seeds,
        secret: String::new(),
        node_id: node_id.to_string(),
        cluster_id: "test-cluster".to_string(),
        kv: ClusterKvConfig::default(),
    };
    start_gossip(&config)
        .await
        .unwrap_or_else(|e| panic!("failed to start gossip for {node_id}: {e}"))
}

/// Poll until all handles see `expected` alive nodes, or panic after `timeout`.
async fn wait_for_convergence(handles: &[&ClusterHandle], expected: usize, timeout: Duration) {
    let start = Instant::now();
    loop {
        let mut all_converged = true;
        for handle in handles {
            let nodes = handle.nodes().await;
            let alive_count = nodes.iter().filter(|n| n.state == NodeState::Alive).count();
            if alive_count != expected {
                all_converged = false;
                break;
            }
        }
        if all_converged {
            return;
        }
        if start.elapsed() > timeout {
            // Print diagnostics before panicking.
            for (i, handle) in handles.iter().enumerate() {
                let nodes = handle.nodes().await;
                eprintln!(
                    "  node {i} ({}) sees {} nodes: {:?}",
                    handle.self_node().id,
                    nodes.len(),
                    nodes,
                );
            }
            panic!("gossip did not converge to {expected} alive nodes within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn single_node_sees_itself() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "solo").await;

    // Give gossip a moment to initialize.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let nodes = node.nodes().await;
    assert_eq!(nodes.len(), 1, "single node should see exactly itself");
    assert_eq!(nodes[0].id, "solo");
    assert_eq!(nodes[0].state, NodeState::Alive);

    node.shutdown().await;
}

#[tokio::test]
async fn three_nodes_discover_each_other() {
    // Allocate 3 random UDP ports.
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let port3 = random_udp_port().await;

    let seed = format!("127.0.0.1:{port1}");

    // Start node-1 with no seeds (it is the seed).
    let node1 = start_node(port1, vec![], "node-1").await;
    // Start node-2 and node-3 pointing at node-1 as seed.
    let node2 = start_node(port2, vec![seed.clone()], "node-2").await;
    let node3 = start_node(port3, vec![seed], "node-3").await;

    // Wait for all three to see each other.
    wait_for_convergence(&[&node1, &node2, &node3], 3, Duration::from_secs(10)).await;

    // Verify all nodes are alive.
    for handle in [&node1, &node2, &node3] {
        let members = handle.nodes().await;
        assert_eq!(members.len(), 3, "{} sees wrong count: {members:?}", handle.self_node().id,);
        assert!(
            members.iter().all(|n| n.state == NodeState::Alive),
            "all nodes should be alive: {members:?}",
        );
    }

    // Verify we see the correct node IDs.
    let mut ids: Vec<String> = node1.nodes().await.iter().map(|n| n.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["node-1", "node-2", "node-3"]);

    node1.shutdown().await;
    node2.shutdown().await;
    node3.shutdown().await;
}

#[tokio::test]
async fn api_nodes_json_serialization() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "json-test").await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify self_node serialization.
    let self_node = node.self_node();
    let json = serde_json::to_value(&self_node).expect("self_node should serialize");
    assert_eq!(json["id"], "json-test");
    assert!(json["gossip_addr"].as_str().is_some());
    assert_eq!(json["state"], "alive");

    // Verify nodes() list serialization.
    let nodes = node.nodes().await;
    let json = serde_json::to_value(&nodes).expect("nodes should serialize");
    let arr = json.as_array().expect("should be array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "json-test");

    // Verify the full API response shape.
    let response = serde_json::json!({
        "self": node.self_node(),
        "cluster_id": node.cluster_id(),
        "nodes": nodes,
    });
    assert_eq!(response["cluster_id"], "test-cluster");
    assert!(response["self"]["id"].is_string());
    assert!(response["nodes"].is_array());

    node.shutdown().await;
}

// ---------------------------------------------------------------------------
// Gossip KV tier tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gossip_kv_set_get_local() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "kv-local").await;

    // Set and immediately read back on the same node.
    node.gossip_set("greeting", b"hello", None).await;
    let value = node.gossip_get("greeting").await;
    assert_eq!(value.as_deref(), Some(b"hello".as_slice()));

    // Key listing.
    let keys = node.gossip_keys().await;
    assert!(keys.contains(&"greeting".to_string()));

    // Exists check.
    assert!(node.gossip_exists("greeting").await);
    assert!(!node.gossip_exists("missing").await);

    // Delete.
    assert!(node.gossip_del("greeting").await);
    assert!(!node.gossip_exists("greeting").await);

    node.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_ttl_expiry() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "kv-ttl").await;

    // Set with a very short TTL.
    node.gossip_set("ephemeral", b"data", Some(Duration::from_millis(200))).await;

    // Should be readable immediately.
    assert!(node.gossip_get("ephemeral").await.is_some());

    // TTL should report remaining time.
    let ttl = node.gossip_pttl("ephemeral").await;
    assert!(ttl.is_some());
    assert!(ttl.unwrap() <= 200);

    // Wait for expiry.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Should be gone.
    assert!(node.gossip_get("ephemeral").await.is_none());

    node.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_binary_data() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "kv-binary").await;

    // Store arbitrary binary data including null bytes.
    let binary: Vec<u8> = (0..=255).collect();
    node.gossip_set("bin", &binary, None).await;

    let value = node.gossip_get("bin").await.expect("should exist");
    assert_eq!(value, binary);

    node.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_replicates_between_nodes() {
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port1}");

    let node1 = start_node(port1, vec![], "kv-a").await;
    let node2 = start_node(port2, vec![seed], "kv-b").await;

    // Wait for cluster convergence first.
    wait_for_convergence(&[&node1, &node2], 2, Duration::from_secs(10)).await;

    // Set a key on node1.
    node1.gossip_set("shared-key", b"from-node1", None).await;

    // Poll until node2 can see it (gossip propagation).
    let start = Instant::now();
    loop {
        if let Some(value) = node2.gossip_get("shared-key").await {
            assert_eq!(value, b"from-node1");
            break;
        }
        assert!(
            start.elapsed() <= Duration::from_secs(10),
            "gossip KV did not replicate within 10s",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Node2 should also see it in its key listing.
    let keys = node2.gossip_keys().await;
    assert!(keys.contains(&"shared-key".to_string()));

    node1.shutdown().await;
    node2.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_delete_not_owned_returns_false() {
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port1}");

    let node1 = start_node(port1, vec![], "del-a").await;
    let node2 = start_node(port2, vec![seed], "del-b").await;

    wait_for_convergence(&[&node1, &node2], 2, Duration::from_secs(10)).await;

    // Node1 sets a key.
    node1.gossip_set("owned-by-1", b"value", None).await;

    // Wait for replication.
    let start = Instant::now();
    loop {
        if node2.gossip_get("owned-by-1").await.is_some() {
            break;
        }
        assert!(start.elapsed() <= Duration::from_secs(10), "key did not replicate",);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Node2 cannot delete it (it doesn't own it).
    assert!(!node2.gossip_del("owned-by-1").await);
    // Node1 can still read it.
    assert!(node1.gossip_get("owned-by-1").await.is_some());

    // Node1 can delete it.
    assert!(node1.gossip_del("owned-by-1").await);

    node1.shutdown().await;
    node2.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_exists_returns_true_for_set_key() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "exists-test").await;

    node.gossip_set("present", b"val", None).await;
    assert!(node.gossip_exists("present").await);
    assert!(!node.gossip_exists("absent").await);

    node.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_pttl_returns_remaining_ms() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "pttl-test").await;

    // Key with TTL
    node.gossip_set("ttl-key", b"val", Some(Duration::from_secs(60))).await;
    let pttl = node.gossip_pttl("ttl-key").await;
    assert!(pttl.is_some(), "TTL key should have pttl");
    let ms = pttl.unwrap();
    assert!(ms > 58_000 && ms <= 60_000, "pttl should be ~60s, got {ms}");

    // Key without TTL
    node.gossip_set("no-ttl", b"val", None).await;
    assert!(node.gossip_pttl("no-ttl").await.is_none(), "key without TTL should return None");

    // Missing key
    assert!(node.gossip_pttl("missing").await.is_none(), "missing key should return None");

    node.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_keys_returns_all_keys() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "keys-test").await;

    node.gossip_set("alpha", b"1", None).await;
    node.gossip_set("beta", b"2", None).await;
    node.gossip_set("gamma", b"3", None).await;

    let keys = node.gossip_keys().await;
    assert!(keys.contains(&"alpha".to_string()));
    assert!(keys.contains(&"beta".to_string()));
    assert!(keys.contains(&"gamma".to_string()));
    assert_eq!(keys.len(), 3);

    node.shutdown().await;
}

#[tokio::test]
async fn gossip_kv_exists_false_after_delete() {
    let port = random_udp_port().await;
    let node = start_node(port, vec![], "del-exists").await;

    node.gossip_set("ephemeral", b"here", None).await;
    assert!(node.gossip_exists("ephemeral").await);

    node.gossip_del("ephemeral").await;
    assert!(!node.gossip_exists("ephemeral").await, "deleted key should not exist");

    node.shutdown().await;
}
