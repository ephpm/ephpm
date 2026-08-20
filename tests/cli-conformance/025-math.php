<?php
var_dump(round(2.5), round(3.5), round(-2.5), round(1.955, 2), round(1234.5678, -2));
var_dump(round(2.5, 0, PHP_ROUND_HALF_DOWN), round(2.5, 0, PHP_ROUND_HALF_EVEN), round(3.5, 0, PHP_ROUND_HALF_ODD));
var_dump(floor(-1.5), ceil(-1.5), floor(1.5), ceil(1.5));
var_dump(abs(-5), abs(-5.5), abs(PHP_INT_MIN + 1));
var_dump(2 ** 10, 2 ** -1, (-2) ** 3, 2 ** 0.5, pow(2, 62), pow(2, 63));
var_dump(sqrt(-1), sqrt(2), log(0), log(-1), log(8, 2), log10(1000), exp(1));
var_dump(fmod(10, 3), fmod(-10, 3), fmod(10.5, 3.2));
var_dump(pi(), M_PI, sin(M_PI), cos(0), tan(M_PI / 4), atan2(1, 1));
var_dump(max(1, "2", 3.5), min([1, "2", 3.5]), max("a", 1), max([], [1]));
var_dump(bindec("1010"), hexdec("ff"), octdec("777"), decbin(10), dechex(255), decoct(8));
var_dump(base_convert("ff", 16, 2), intval("0x1A", 16), intval("0x1A", 0), intval("012", 0));
var_dump((int)((0.1 + 0.7) * 10));
var_dump(PHP_FLOAT_DIG, PHP_EOL === "\n");
