<?php
$a = [3, 1, 2, 10, "5", 2.5, "07"];
sort($a);
var_dump($a);
$a = [3, 1, 2, 10, "5", 2.5, "07"];
rsort($a);
var_dump($a);
$a = ["b" => 2, "a" => 1, "c" => 3, "A" => 0];
asort($a);
var_dump($a);
ksort($a);
var_dump($a);
$a = ["10" => "a", "9" => "b", "x" => "c", "1.5" => "d"];
ksort($a);
var_dump($a);
// PHP >= 8.0 sorts are stable: equal keys keep insertion order.
$items = [["k" => 1, "v" => "first"], ["k" => 0, "v" => "z"], ["k" => 1, "v" => "second"], ["k" => 1, "v" => "third"]];
usort($items, fn($x, $y) => $x["k"] <=> $y["k"]);
foreach ($items as $i) {
    echo $i["v"], " ";
}
echo "\n";
$mixed = [0, "0", "", null, false, "a", [], true];
sort($mixed);
var_dump($mixed);
$n = ["img12", "img10", "img2", "IMG1"];
natsort($n);
var_dump($n);
natcasesort($n);
var_dump($n);
$m = [3 => "c", 1 => "a", 2 => "b"];
uksort($m, fn($x, $y) => $y <=> $x);
var_dump($m);
$d1 = [3, 1, 3];
$d2 = ["c", "a", "b"];
array_multisort($d1, $d2);
var_dump($d1, $d2);
