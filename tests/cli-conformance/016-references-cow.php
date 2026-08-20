<?php
$a = [1, 2, 3];
$b = $a;
$b[] = 4;
var_dump(count($a), count($b));
$r = &$a[1];
$c = $a; // array containing a ref slot: the slot is shared by the copy
$r = 99;
var_dump($a[1], $c[1]);
function mod(array &$x): void
{
    $x[0] = "modified";
}
mod($a);
var_dump($a[0]);
$s1 = "orig";
$s2 = &$s1;
$s2 .= "+more";
var_dump($s1);
$arr = ["k" => ["nested" => 1]];
$ref = &$arr["k"]["nested"];
$copy = $arr;
$ref = 2;
var_dump($arr["k"]["nested"], $copy["k"]["nested"]);
unset($ref);
$arr["k"]["nested"] = 3;
var_dump($copy["k"]["nested"]);
foreach ($a as &$v) {
    $v = "x";
}
unset($v);
var_dump($a);
$obj = new stdClass();
$obj->p = 1;
$o2 = $obj;
$o2->p = 2;
var_dump($obj->p);
$o3 = clone $obj;
$o3->p = 3;
var_dump($obj->p, $o3->p);
