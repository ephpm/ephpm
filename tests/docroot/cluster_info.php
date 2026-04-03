<?php
/**
 * Returns JSON with cluster node information.
 *
 * Response fields:
 *   - hostname:  system hostname (stable in a StatefulSet)
 *   - node_id:   HOSTNAME env var (set by Kubernetes for StatefulSet pods)
 *   - php_sapi:  PHP SAPI name
 *   - pid:       current process ID
 */

header('Content-Type: application/json');

echo json_encode([
    'hostname' => gethostname(),
    'node_id'  => getenv('HOSTNAME') ?: '',
    'php_sapi' => php_sapi_name(),
    'pid'      => getmypid(),
]);
