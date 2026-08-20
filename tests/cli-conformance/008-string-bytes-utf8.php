<?php
$u = "h\u{E9}llo w\u{F6}rld \u{2713}";
var_dump(strlen($u), strtoupper($u), ucfirst($u), strrev("abc"), str_split($u, 3));
$b = "\x00\x01\xFF\xFEbin";
var_dump(strlen($b), bin2hex($b), addslashes("a'b\"c\\d\0e"));
var_dump(substr($u, 0, 5), substr($u, -3), substr("abc", 5), substr("abc", 1, -1));
var_dump(strpos($u, "\u{F6}"), strrpos("aXbXc", "X"), str_contains($u, "\u{2713}"), stripos("AbC", "b"));
var_dump(trim("  x  "), rtrim("xyyy", "y"), ltrim("//x", "/"), trim("a..b", ".ab"));
var_dump(str_pad("7", 5, "0", STR_PAD_LEFT), str_repeat("ab", 3), wordwrap("The quick brown fox", 10, "|", true));
var_dump(ucwords("hello world-foo bar_baz", " -_"), lcfirst("ABC"), chunk_split("abcdef", 2, "-"));
var_dump(strtr("abcdef", "abc", "xyz"), strtr("hi all", ["hi" => "bye", "all" => "everyone"]));
var_dump(nl2br("a\nb\r\nc"), htmlspecialchars("<a href='x'>&\"</a>"), htmlspecialchars("<x>", ENT_NOQUOTES), html_entity_decode("&lt;&amp;&gt;"));
var_dump(levenshtein("kitten", "sitting"), similar_text("World", "Word"), soundex("Robert"), metaphone("Thompson"));
