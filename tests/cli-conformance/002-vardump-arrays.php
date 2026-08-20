<?php
var_dump([]);
var_dump([1, 2, 3]);
var_dump(["a" => 1, 2 => "b", "3" => true, -1 => null]);
var_dump([[1, [2, [3, []]]]]);
$a = [1, 2];
$a[] = $a;
var_dump($a);
var_dump(["0" => "zero-string-key", 1.9 => "float-key", true => "bool-key", null => "null-key"]);
$neg = [-5 => "neg"];
$neg[] = "appended-after-negative";
var_dump($neg);
