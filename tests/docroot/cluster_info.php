<?php
/**
 * Returns JSON with cluster node information.
 *
 * Response fields:
 *   - node_id:   this node's distinct cluster identity. Sourced from
 *                $_SERVER['EPHPM_NODE_ID'], which ephpm populates from the
 *                running gossip node id ([cluster] node_id, or an auto-derived
 *                per-node value when left empty). This is distinct per node in
 *                BOTH the bare-process harness (each process on 127.0.0.1) and
 *                a Kubernetes StatefulSet (each pod) -- unlike the OS hostname,
 *                which collapses to one value when every node is a process on
 *                the same host. Falls back to the HOSTNAME env / gethostname()
 *                only if ephpm did not inject the id.
 *   - hostname:  system hostname (kept for backwards-compat / debugging;
 *                NOT a reliable per-node discriminator under bare-process).
 *   - php_sapi:  PHP SAPI name
 *   - pid:       current process ID
 */

header('Content-Type: application/json');

$node_id = $_SERVER['EPHPM_NODE_ID']
    ?? (getenv('EPHPM_NODE_ID') ?: (getenv('HOSTNAME') ?: gethostname()));

echo json_encode([
    'node_id'  => $node_id,
    'hostname' => gethostname(),
    'php_sapi' => php_sapi_name(),
    'pid'      => getmypid(),
]);
