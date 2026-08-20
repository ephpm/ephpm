<?php
var_export(null);
echo "\n";
var_export([1, 2.5, "s", true, null, ["k" => "v"]]);
echo "\n";
var_export(["a" => ["b" => ["c" => 1]]]);
echo "\n";
var_export("quotes ' \" \\ and \n newline");
echo "\n";
var_export(1.0);
echo "\n";
var_export(0.1 + 0.2);
echo "\n";
var_export(-INF);
echo "\n";
var_export(NAN);
echo "\n";
var_export(PHP_INT_MIN);
echo "\n";
$o = new stdClass();
$o->a = [1];
var_export($o);
echo "\n";
class VE
{
    public $x = 1;
    private $y = 2;
}
var_export(new VE());
echo "\n";
echo var_export("as-string", true), "\n";
print_r([1, ["k" => "v"], new stdClass()]);
echo "\n";
echo print_r("scalar", true), "\n";
print_r(true);
print_r(false);
print_r(null);
echo "|\n";
