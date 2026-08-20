<?php
$pairs = [
    [0, "a"], [0, ""], [0, "0"], [0, null], ["1", "01"], ["10", "1e1"], [100, "1e2"],
    ["abc", 0], [null, false], [null, 0], [1, true], [-1, true], ["", null], ["null", null],
    [[], false], [[], null], ["1", true], ["0", false], [0.5, "0.5"], ["0.50", "0.5"],
];
foreach ($pairs as [$l, $r]) {
    printf(
        "%s == %s : %s | === : %s | <=> %d\n",
        var_export($l, true),
        var_export($r, true),
        var_export($l == $r, true),
        var_export($l === $r, true),
        $l <=> $r
    );
}
var_dump((int)"12abc", (int)"abc", (int)"0x1A", (int)"1e3", (float)"1e3", (int)12.9, (int)-12.9);
var_dump((bool)"0.0", (bool)" ", (bool)[0], (string)false, (string)null, (string)1.0);
var_dump(0 == "", 0 == null, "0" == false, "" == null);
$s = "z";
$s++;
var_dump($s);
$s2 = "Az";
$s2++;
var_dump($s2);
$s3 = "a9";
$s3++;
var_dump($s3);
var_dump(1 + "5 apples");
var_dump(is_numeric("1e5"), is_numeric("0x1A"), is_numeric(" 5"), is_numeric("5 "), is_numeric(".5"), is_numeric("5."));
