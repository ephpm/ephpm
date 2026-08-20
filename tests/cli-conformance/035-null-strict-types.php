<?php
$o = null;
var_dump($o?->prop, $o?->method());
$set = ["a" => null, "b" => 0];
var_dump($set["a"] ?? "dflt", $set["b"] ?? "dflt", $set["c"] ?? "dflt");
$x = null;
$x ??= "assigned";
var_dump($x);
$y = "kept";
$y ??= "not";
var_dump($y);
var_dump(is_null(null), null == false, null === false, (int)null, (string)null === "");
function typed(?int $i): ?string
{
    return $i === null ? null : (string)$i;
}
var_dump(typed(null), typed(5));
try {
    typed("abc");
} catch (TypeError $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
function needsInt(int $i): int
{
    return $i;
}
var_dump(needsInt("42"));
var_dump(needsInt(4.0));
try {
    var_dump(needsInt(4.5));
} catch (TypeError $e) {
    echo "TypeError(float): ", $e->getMessage(), "\n";
}
function variadic(int ...$nums): int
{
    return array_sum($nums);
}
var_dump(variadic(1, 2, 3), variadic(...[4, 5]));
function defaults($a, $b = "b-default", ...$rest)
{
    return func_get_args();
}
var_dump(defaults("A"), defaults("A", "B", "C", "D"), func_num_args());
var_dump(gettype(1), gettype(1.5), gettype("s"), gettype([]), gettype(null), gettype(new stdClass()), get_debug_type(1.5), get_debug_type(null));
