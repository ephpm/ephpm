<?php
if (!extension_loaded('mbstring')) {
    echo "mbstring: MISSING\n";
    exit(0);
}
$u = "h\u{E9}llo w\u{F6}rld \u{2713} \u{3053}\u{3093}\u{306B}\u{3061}\u{306F}";
var_dump(mb_strlen($u), mb_substr($u, 6, 5), mb_strtoupper($u), mb_strtolower("\u{C4}\u{D6}\u{DC}"), mb_str_split("\u{65E5}\u{672C}\u{8A9E}"));
var_dump(mb_strpos($u, "\u{2713}"), mb_strwidth($u), mb_convert_case("hello w\u{F6}rld", MB_CASE_TITLE));
var_dump(mb_detect_encoding($u), mb_check_encoding("\xFF\xFE", 'UTF-8'), mb_scrub("a\xFFb"));
var_dump(bin2hex(mb_convert_encoding("h\u{E9}llo", 'ISO-8859-1', 'UTF-8')), mb_internal_encoding());
