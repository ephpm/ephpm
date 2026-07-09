<?php
// OPcache status probe used by the opcache_invalidation e2e test.
//
// By default warms a sibling target file (include_once) so its bytecode is
// guaranteed to be in the OPcache, then reports cache state. Pass ?warm=0 to
// OBSERVE without warming — required to see that an invalidation actually
// dropped the entry (a warming probe would immediately re-cache it).
//
// `target_cached` uses opcache_is_script_cached(): opcache_invalidate()
// keeps the entry listed in opcache_get_status()['scripts'] but marks it
// unusable, so presence-in-list is NOT a valid cached signal.
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

$warm = !isset($_GET['warm']) || $_GET['warm'] !== '0';
if ($warm && is_file($target)) {
    include_once $target;
}

$response = [
    'opcache_enabled' => function_exists('opcache_get_status')
        && is_array(@opcache_get_status(false)),
    'target' => $target,
    'target_cached' => function_exists('opcache_is_script_cached')
        && opcache_is_script_cached($target),
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
        }
        $response['docroot_scripts'] = $docroot_count;
    }
}

echo json_encode($response);
