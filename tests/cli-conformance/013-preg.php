<?php
var_dump(preg_match('/\p{L}+/u', "h\u{E9}llo123", $m), $m);
var_dump(preg_match_all('/\d+/', "a1b22c333", $m), $m);
var_dump(preg_replace('/(a)(b)/', '$2$1', "abab"));
var_dump(preg_split('/[\s,]+/', "a, b  c,d"));
var_dump(preg_split('//u', "a\u{F1}c", -1, PREG_SPLIT_NO_EMPTY));
var_dump(preg_match('/(?<word>\w+)\s+\1/', "hey hey", $m), $m);
var_dump(preg_match('/b/', 'abc', $m, PREG_OFFSET_CAPTURE), $m);
var_dump(preg_quote("1.5-2*3?"), preg_quote("a/b", "/"));
var_dump(preg_replace_callback('/\d/', fn($mm) => $mm[0] * 2, "1a2b3"));
var_dump(preg_grep('/^\d+$/', ["1", "a", "22", "3b"]));
ini_set('pcre.backtrack_limit', '100');
var_dump(preg_match('/(a+)+$/', str_repeat('a', 200) . 'b'));
echo preg_last_error(), " ", preg_last_error_msg(), "\n";
var_dump(@preg_match('/unclosed(/', 'x'));
preg_match('/bad(/', 'x');
var_dump(preg_match('/^\X$/u', "e\u{0301}"));
