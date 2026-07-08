<?php
// OPcache status probe used by the opcache_invalidation e2e test.
//
// Loads a sibling target file (or the file requested via ?script=) so the
// target's bytecode is guaranteed to be in the OPcache, then reports the
// number of scripts currently cached under $DOCUMENT_ROOT plus whether the
// target's own entry is present.
//
// Response shape (JSON):
// {
//   "opcache_enabled": true|false,
//   "target": "/var/www/html/opcache_target.php",
//   "target_cached": true|false,
//   "total_scripts": 42,
//   "docroot_scripts": 40
// }

header('Content-Type: application/json');

$target = $_SERVER['DOCUMENT_ROOT'] . '/opcache_target.php';
if (isset($_GET['script'])) {
    $override = $_SERVER['DOCUMENT_ROOT'] . '/' . ltrim($_GET['script'], '/');
    if (is_file($override)) {
        $target = $override;
    }
}

// Warm the target so it enters the OPcache. include_once is a no-op after the
// first inclusion within this request; the OPcache still records the entry.
if (is_file($target)) {
    include_once $target;
}

$response = [
    'opcache_enabled' => function_exists('opcache_get_status'),
    'target' => $target,
    'target_cached' => false,
    'total_scripts' => 0,
    'docroot_scripts' => 0,
];

if ($response['opcache_enabled']) {
    $status = @opcache_get_status(true);
    if (is_array($status) && isset($status['scripts']) && is_array($status['scripts'])) {
        $response['total_scripts'] = count($status['scripts']);
        $docroot = $_SERVER['DOCUMENT_ROOT'];
        $docroot_count = 0;
        foreach ($status['scripts'] as $path => $_info) {
            if (strpos($path, $docroot) === 0) {
                $docroot_count++;
            }
            if ($path === $target) {
                $response['target_cached'] = true;
            }
        }
        $response['docroot_scripts'] = $docroot_count;
    }
}

echo json_encode($response);
