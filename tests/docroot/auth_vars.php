<?php
/**
 * HTTP authentication $_SERVER variables (issue #386).
 *
 * Reports the keys a stock PHP SAPI derives from an incoming `Authorization`
 * header. Each line distinguishes "absent" from "present but empty", because
 * the two mean different things: php-src leaves PHP_AUTH_PW unset for an empty
 * password, while an empty *username* is registered as an empty string, and
 * applications branch on `isset()`.
 */

header('Content-Type: text/plain');

$keys = [
    'AUTH_TYPE',
    'PHP_AUTH_USER',
    'PHP_AUTH_PW',
    'PHP_AUTH_DIGEST',
    'REMOTE_USER',
    'HTTP_AUTHORIZATION',
];

foreach ($keys as $key) {
    if (array_key_exists($key, $_SERVER)) {
        echo $key . ' = [' . $_SERVER[$key] . "]\n";
    } else {
        echo $key . " unset\n";
    }
}
