<?php
foreach (["abc", "ABC1", "123", " ", "\t\n", "", "\u{E1}\u{E9}"] as $s) {
    printf(
        "%s: alpha=%d alnum=%d digit=%d space=%d upper=%d punct=%d xdigit=%d\n",
        var_export($s, true),
        (int)ctype_alpha($s),
        (int)ctype_alnum($s),
        (int)ctype_digit($s),
        (int)ctype_space($s),
        (int)ctype_upper($s),
        (int)ctype_punct($s),
        (int)ctype_xdigit($s)
    );
}
var_dump(ctype_digit("48"), ctype_xdigit("ff"), ctype_lower("abc"));
