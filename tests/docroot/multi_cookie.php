<?php
// Sets multiple Set-Cookie headers.
// Used by e2e tests to verify multiple headers with the same name are preserved.
header('Content-Type: text/plain');
setcookie('session', 'abc123', ['path' => '/', 'httponly' => true]);
setcookie('theme', 'dark', ['path' => '/']);
setcookie('lang', 'en', ['path' => '/']);
echo "cookies set\n";
