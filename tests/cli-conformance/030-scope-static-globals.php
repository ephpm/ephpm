<?php
$g = "global";
function readsGlobal()
{
    global $g;
    echo $g, "\n";
}
readsGlobal();
function counter()
{
    static $n = 0;
    return ++$n;
}
echo counter(), counter(), counter(), "\n";
function &refReturn(array &$arr)
{
    return $arr[0];
}
$a = [1];
$r = &refReturn($a);
$r = 5;
echo $a[0], "\n";
echo isset($GLOBALS['g']) ? "g set" : "g unset", "\n";
$GLOBALS['h'] = "via-globals";
echo $h, "\n";
class StaticP
{
    public static $sp = "static-prop";
    const C = "const";
}
echo StaticP::$sp, " ", StaticP::C, " ", constant('StaticP::C'), "\n";
define('RUNTIME_CONST', 42);
echo RUNTIME_CONST, " ", defined('RUNTIME_CONST') ? "def" : "undef", "\n";
var_dump(function_exists('readsGlobal'), function_exists('nope'), class_exists('StaticP'), class_exists('Nope'));
