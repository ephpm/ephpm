//! Stress tests for gossip clustering.
//!
//! All tests are `#[ignore = "nightly CI only — timing-sensitive gossip tests"]` so they only run during nightly CI
//! (via `cargo test -- --ignored` or `cargo nextest -E 'test(#ignored)'`).
//!
//! These tests exercise convergence under larger cluster sizes,
//! failure detection, and KV replication at scale.

use std::fmt::Write;
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
        cluster_id: "stress-test-cluster".to_string(),
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
                let alive = nodes.iter().filter(|n| n.state == NodeState::Alive).count();
                eprintln!(
                    "  node {i} ({}) sees {alive}/{} alive nodes: {:?}",
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

/// Start 5 nodes daisy-chained (node N seeds on node N-1). Wait for all
/// nodes to discover all others within 15 seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "nightly CI only — timing-sensitive gossip tests"]
async fn multi_node_convergence() {
    const NODE_COUNT: usize = 5;
    const TIMEOUT: Duration = Duration::from_secs(15);

    // Allocate all ports up front to avoid race conditions.
    let mut ports = Vec::with_capacity(NODE_COUNT);
    for _ in 0..NODE_COUNT {
        ports.push(random_udp_port().await);
    }

    // Start nodes with daisy-chain seeding: node 0 has no seeds,
    // node N seeds on node N-1.
    let mut handles = Vec::with_capacity(NODE_COUNT);
    for (i, &port) in ports.iter().enumerate() {
        let seeds = if i == 0 { vec![] } else { vec![format!("127.0.0.1:{}", ports[i - 1])] };
        let node_id = format!("stress-{i}");
        handles.push(start_node(port, seeds, &node_id).await);
    }

    // Wait for full convergence: every node sees all 5 alive.
    let refs: Vec<&ClusterHandle> = handles.iter().collect();
    wait_for_convergence(&refs, NODE_COUNT, TIMEOUT).await;

    // Verify each node sees exactly the right set of node IDs.
    let mut expected_ids: Vec<String> = (0..NODE_COUNT).map(|i| format!("stress-{i}")).collect();
    expected_ids.sort();

    for handle in &handles {
        let nodes = handle.nodes().await;
        assert_eq!(
            nodes.len(),
            NODE_COUNT,
            "{} sees {} nodes, expected {NODE_COUNT}: {nodes:?}",
            handle.self_node().id,
            nodes.len(),
        );
        assert!(
            nodes.iter().all(|n| n.state == NodeState::Alive),
            "{} sees dead nodes: {nodes:?}",
            handle.self_node().id,
        );
        let mut ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, expected_ids, "{} sees wrong node IDs", handle.self_node().id);
    }

    // Clean shutdown.
    for handle in handles {
        handle.shutdown().await;
    }
}

/// Start 3 nodes, wait for convergence, drop node 2, then verify the
/// remaining 2 nodes detect the failure within 30 seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "nightly CI only — timing-sensitive gossip tests"]
async fn node_failure_detection() {
    const TIMEOUT_CONVERGE: Duration = Duration::from_secs(10);
    const TIMEOUT_FAILURE: Duration = Duration::from_secs(30);

    let port0 = random_udp_port().await;
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port0}");

    let node0 = start_node(port0, vec![], "fail-0").await;
    let node1 = start_node(port1, vec![seed.clone()], "fail-1").await;
    let node2 = start_node(port2, vec![seed], "fail-2").await;

    // Wait for all 3 to see each other.
    wait_for_convergence(&[&node0, &node1, &node2], 3, TIMEOUT_CONVERGE).await;

    // Drop node2 — this shuts down its gossip listener, simulating a crash.
    drop(node2);

    // Wait for nodes 0 and 1 to detect the failure: node2 should disappear
    // from their live nodes list (either absent or marked Dead).
    let start = Instant::now();
    loop {
        let n0_alive = node0.nodes().await.iter().filter(|n| n.state == NodeState::Alive).count();
        let n1_alive = node1.nodes().await.iter().filter(|n| n.state == NodeState::Alive).count();

        // Both survivors should see exactly 2 alive nodes (themselves + the
        // other survivor). Node2 should be dead or gone.
        if n0_alive == 2 && n1_alive == 2 {
            break;
        }

        if start.elapsed() > TIMEOUT_FAILURE {
            let n0_nodes = node0.nodes().await;
            let n1_nodes = node1.nodes().await;
            eprintln!("  node0 sees: {n0_nodes:?}");
            eprintln!("  node1 sees: {n1_nodes:?}");
            panic!(
                "failure detection did not complete within {TIMEOUT_FAILURE:?}: \
                 node0 alive={n0_alive}, node1 alive={n1_alive}"
            );
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Verify node2 is either marked Dead or no longer listed.
    for handle in [&node0, &node1] {
        let members = handle.nodes().await;
        let node2_entry = members.iter().find(|n| n.id == "fail-2");
        if let Some(entry) = node2_entry {
            assert_eq!(
                entry.state,
                NodeState::Dead,
                "{} still shows fail-2 as alive",
                handle.self_node().id,
            );
        }
        // If node2 is not in the list at all, that is also acceptable.
    }

    node0.shutdown().await;
    node1.shutdown().await;
}

/// Start 3 nodes, wait for convergence. Node 0 sets 50 KV entries via
/// gossip. Wait up to 30 seconds for all entries to replicate to all nodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "nightly CI only — timing-sensitive gossip tests"]
async fn kv_replication() {
    const NODE_COUNT: usize = 3;
    const KEY_COUNT: usize = 50;
    const TIMEOUT_CONVERGE: Duration = Duration::from_secs(10);
    const TIMEOUT_REPLICATE: Duration = Duration::from_secs(30);

    let port0 = random_udp_port().await;
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port0}");

    let node0 = start_node(port0, vec![], "kv-stress-0").await;
    let node1 = start_node(port1, vec![seed.clone()], "kv-stress-1").await;
    let node2 = start_node(port2, vec![seed], "kv-stress-2").await;

    let handles = [&node0, &node1, &node2];

    // Wait for cluster convergence.
    wait_for_convergence(&handles, NODE_COUNT, TIMEOUT_CONVERGE).await;

    // Node 0 writes 50 KV entries.
    for i in 0..KEY_COUNT {
        let key = format!("stress-key-{i:03}");
        let value = format!("value-{i:03}");
        node0.gossip_set(&key, value.as_bytes(), None).await;
    }

    // Poll until all 3 nodes can read all 50 entries.
    let start = Instant::now();
    loop {
        let mut all_replicated = true;
        let mut missing_report = String::new();

        for (node_idx, handle) in handles.iter().enumerate() {
            let mut missing_count = 0usize;
            for i in 0..KEY_COUNT {
                let key = format!("stress-key-{i:03}");
                if handle.gossip_get(&key).await.is_none() {
                    missing_count += 1;
                }
            }
            if missing_count > 0 {
                all_replicated = false;
                let _ = writeln!(
                    missing_report,
                    "  node {node_idx} ({}) missing {missing_count}/{KEY_COUNT} keys",
                    handle.self_node().id,
                );
            }
        }

        if all_replicated {
            break;
        }

        assert!(
            start.elapsed() <= TIMEOUT_REPLICATE,
            "KV replication did not complete within {TIMEOUT_REPLICATE:?}:\n{missing_report}"
        );

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Final verification: check every key on every node has the correct value.
    for handle in &handles {
        for i in 0..KEY_COUNT {
            let key = format!("stress-key-{i:03}");
            let expected = format!("value-{i:03}");
            let actual = handle.gossip_get(&key).await.unwrap_or_else(|| {
                panic!("{} missing key {key} after replication", handle.self_node().id)
            });
            assert_eq!(
                actual,
                expected.as_bytes(),
                "{} has wrong value for {key}",
                handle.self_node().id,
            );
        }
    }

    // Verify gossip_keys() returns all 50 keys on each node.
    for handle in &handles {
        let keys = handle.gossip_keys().await;
        assert!(
            keys.len() >= KEY_COUNT,
            "{} gossip_keys() returned {} keys, expected at least {KEY_COUNT}",
            handle.self_node().id,
            keys.len(),
        );
        for i in 0..KEY_COUNT {
            let key = format!("stress-key-{i:03}");
            assert!(keys.contains(&key), "{} gossip_keys() missing {key}", handle.self_node().id);
        }
    }

    node0.shutdown().await;
    node1.shutdown().await;
    node2.shutdown().await;
}
