<?php
// Lowers the memory limit then allocates past it.
// Server must return 500 without hanging or crashing.
ini_set('memory_limit', '2M');
$chunks = [];
for ($i = 0; $i < 100; $i++) {
    $chunks[] = str_repeat('x', 1024 * 1024); // 1 MB per iteration
}
echo "should not reach here";
