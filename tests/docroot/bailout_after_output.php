<?php
// A script that produces real output and a real header, and THEN dies in a
// Zend bailout (memory exhaustion). php_execute_script's own zend_try absorbs
// the bailout, so nothing unwinds out to ephpm's SETJMP guard and the request
// looks, from the outside, like it simply returned.
//
// The contract: none of what is below the marker exists, so none of it may be
// delivered. The client must get a 500 and must NOT get the marker, the
// header, or a 200.
header('X-Bailout-Fixture: emitted-before-the-bailout');
echo "BAILOUT-FIXTURE-PARTIAL-OUTPUT\n";
// Force the allocator past the limit -> zend_error_noreturn(E_ERROR) -> bailout.
ini_set('memory_limit', '2M');
$chunks = [];
for ($i = 0; $i < 100; $i++) {
    $chunks[] = str_repeat('x', 1024 * 1024); // 1 MB per iteration
}
echo "BAILOUT-FIXTURE-UNREACHABLE\n";
