<?php
header('Content-Type: application/json');
$status = function_exists('opcache_get_status') ? opcache_get_status(false) : null;
echo json_encode([
    'ext_loaded' => extension_loaded('Zend OPcache'),
    'ini_enable' => ini_get('opcache.enable'),
    'enabled' => is_array($status) ? ($status['opcache_enabled'] ?? false) : false,
    'sapi' => php_sapi_name(),
]);
