<?php
$s = "The quick brown fox\n";
foreach (['md5', 'sha1', 'sha256', 'sha512', 'crc32b', 'xxh32', 'xxh64', 'xxh3', 'xxh128', 'fnv1a64', 'adler32', 'murmur3a'] as $algo) {
    echo $algo, "=", hash($algo, $s), "\n";
}
echo md5(''), " ", sha1(''), " ", crc32(''), " ", crc32($s), "\n";
echo hash_hmac('sha256', 'msg', 'key'), "\n";
echo base64_encode(hash('sha256', $s, true)), "\n";
var_dump(hash_equals('abc', 'abc'), hash_equals('abc', 'abd'));
$ctx = hash_init('sha256');
hash_update($ctx, 'part1');
hash_update($ctx, 'part2');
echo hash_final($ctx), "\n";
var_dump(password_verify('secret', '$2y$10$abcdefghijklmnopqrstuv'));
