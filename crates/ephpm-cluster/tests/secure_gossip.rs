//! Integration tests for encrypted gossip (`[cluster] secret`).
//!
//! Verifies that nodes sharing a secret form a cluster over the
//! encrypted UDP transport, and that a node with the wrong secret (or
//! no secret at all) is invisible to the cluster — it can neither join
//! nor be joined.

use std::net::SocketAddr;
use std::time::Duration;

use ephpm_cluster::{ClusterHandle, NodeState, start_gossip};
use ephpm_config::ClusterConfig;

/// Find a free UDP port by binding to port 0 and returning the address.
fn free_udp_addr() -> SocketAddr {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind to port 0");
    sock.local_addr().expect("local addr")
}

/// Start a gossip node on a fresh random port, retrying on bind
/// collisions (the port from [`free_udp_addr`] is released before use,
/// so parallel tests can race for it).
async fn start_node(
    node_id: &str,
    cluster_id: &str,
    seeds: Vec<String>,
    secret: &str,
) -> ClusterHandle {
    let mut last_err = None;
    for _ in 0..10 {
        let config = ClusterConfig {
            enabled: true,
            bind: free_udp_addr().to_string(),
            join: seeds.clone(),
            secret: secret.to_string(),
            node_id: node_id.to_string(),
            cluster_id: cluster_id.to_string(),
            ..ClusterConfig::default()
        };
        match start_gossip(&config).await {
            Ok(handle) => return handle,
            Err(e) => last_err = Some(e),
        }
    }
    panic!("{node_id} failed to start after 10 attempts: {last_err:?}");
}

#[tokio::test]
async fn nodes_with_matching_secret_converge() {
    let handle1 = start_node("enc-a", "test-encrypted", vec![], "shared-secret").await;
    let seed = handle1.self_node().gossip_addr;

    let handle2 = start_node("enc-b", "test-encrypted", vec![seed], "shared-secret").await;

    // Wait for gossip convergence over the encrypted transport.
    let mut discovered = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if handle1.live_node_count().await >= 2 {
            discovered = true;
            break;
        }
    }
    assert!(discovered, "nodes with matching secrets should discover each other");

    // KV must propagate through the encrypted channel too.
    handle1.gossip_set("enc-key", b"enc-value", None).await;
    let mut found = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(value) = handle2.gossip_get("enc-key").await {
            assert_eq!(value, b"enc-value");
            found = true;
            break;
        }
    }
    assert!(found, "KV should propagate over encrypted gossip");

    handle1.shutdown().await;
    handle2.shutdown().await;
}

#[tokio::test]
async fn wrong_secret_node_cannot_join() {
    let handle1 = start_node("sec-good", "test-wrong-secret", vec![], "right-secret").await;
    let seed = handle1.self_node().gossip_addr;

    // Same cluster_id, wrong secret, seeds at the good node.
    let handle2 = start_node("sec-bad", "test-wrong-secret", vec![seed], "wrong-secret").await;

    // Give gossip plenty of rounds to (incorrectly) converge.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The good node must not have learned about the intruder…
    let good_view = handle1.nodes().await;
    assert!(
        !good_view.iter().any(|n| n.id == "sec-bad"),
        "node with the wrong secret must be invisible, got {good_view:?}"
    );

    // …and the intruder must not have learned about the cluster.
    let bad_view = handle2.nodes().await;
    let alive: Vec<_> = bad_view.iter().filter(|n| n.state == NodeState::Alive).collect();
    assert_eq!(alive.len(), 1, "wrong-secret node should only see itself, got {bad_view:?}");
    assert_eq!(alive[0].id, "sec-bad");

    handle1.shutdown().await;
    handle2.shutdown().await;
}

#[tokio::test]
async fn plaintext_node_cannot_join_encrypted_cluster() {
    let handle1 = start_node("pt-good", "test-plaintext-mix", vec![], "cluster-secret").await;
    let seed = handle1.self_node().gossip_addr;

    // No secret at all → plaintext UDP transport.
    let handle2 = start_node("pt-bare", "test-plaintext-mix", vec![seed], "").await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    let good_view = handle1.nodes().await;
    assert!(
        !good_view.iter().any(|n| n.id == "pt-bare"),
        "plaintext node must be invisible to an encrypted cluster, got {good_view:?}"
    );

    let bare_view = handle2.nodes().await;
    let alive: Vec<_> = bare_view.iter().filter(|n| n.state == NodeState::Alive).collect();
    assert_eq!(alive.len(), 1, "plaintext node should only see itself, got {bare_view:?}");

    handle1.shutdown().await;
    handle2.shutdown().await;
}
