<?php

declare(strict_types=1);

// E2E probe for the extension-loading test (tests/extensions.rs). Served as
// /ext-probe.php by a fixture whose [php] extensions = [...] points at the
// ePHPm ZTS extension catalog .so files. Reports which catalog extensions
// loaded and whether each is FUNCTIONAL (a real round-trip), not merely
// present. See tests/extensions.rs for the fixture recipe.

header('Content-Type: application/json');

$want = ['igbinary', 'msgpack', 'apcu', 'redis', 'mongodb'];

$out = ['loaded' => [], 'functional' => []];

foreach ($want as $e) {
    if (extension_loaded($e)) {
        $out['loaded'][] = $e;
    }
}

if (function_exists('igbinary_serialize')) {
    $r = igbinary_unserialize(igbinary_serialize(['x' => [1, 2, 3]]));
    $out['functional']['igbinary'] = ($r['x'][2] === 3);
}

if (function_exists('msgpack_pack')) {
    $out['functional']['msgpack'] = (msgpack_unpack(msgpack_pack(['a' => 7]))['a'] === 7);
}

if (function_exists('apcu_store')) {
    apcu_store('ext_probe_k', 'v');
    $out['functional']['apcu'] = (apcu_fetch('ext_probe_k') === 'v');
}

if (class_exists('Redis')) {
    $out['functional']['redis_class'] = true;
}

if (class_exists('MongoDB\\Driver\\Manager')) {
    $out['functional']['mongodb_class'] = true;
}

echo json_encode($out, JSON_PRETTY_PRINT), "\n";
