<?php
var_dump(PHP_INT_MAX, PHP_INT_MAX + 1, PHP_INT_MIN, PHP_INT_MIN - 1);
var_dump(9223372036854775807, 9223372036854775808);
var_dump(intdiv(7, 2), intdiv(-7, 2), intdiv(7, -2), intdiv(-7, -2));
var_dump(7 % 3, -7 % 3, 7 % -3, -7 % -3);
var_dump(fmod(-7.5, 3), fdiv(1, 0), fdiv(-1, 0), fdiv(0, 0));
try {
    intdiv(PHP_INT_MIN, -1);
} catch (ArithmeticError $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
try {
    echo 1 % 0;
} catch (DivisionByZeroError $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
var_dump(0x7FFFFFFFFFFFFFFF, 0o777, 0b1010, 0777, 1_000_000);
var_dump((int)"9223372036854775807", (int)"9223372036854775808", (float)"9223372036854775808");
