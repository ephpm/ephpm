<?php
// Outputs slightly over 1 MiB of text. Used by compression and body-size tests.
// The content is deterministic so tests can verify it decompresses correctly.
header('Content-Type: text/plain');
$chunk = str_repeat("ephpm-large-output-test-line\n", 36);  // ~1 KiB per iteration
for ($i = 0; $i < 1025; $i++) {
    echo $chunk;
}
