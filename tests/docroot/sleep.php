<?php
// Sleeps for the number of seconds given in ?seconds=N (float, max 60).
// Used by timeout tests to trigger request-timeout behaviour.
header('Content-Type: text/plain');
$seconds = (float) ($_GET['seconds'] ?? 1);
$seconds = min($seconds, 60.0);
usleep((int) ($seconds * 1_000_000));
echo "slept {$seconds}s\n";
