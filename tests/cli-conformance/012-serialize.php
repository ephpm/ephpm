<?php
$data = ["int" => 1, "float" => 1.5, "str" => "s", "nested" => [true, null, ["deep" => PHP_INT_MAX]]];
$s = serialize($data);
echo $s, "\n";
var_dump(unserialize($s) === $data);
class Ser {
    public $a = 1;
    protected $b = "two";
    private $c = [3];
}
echo serialize(new Ser()), "\n";
var_dump(unserialize(serialize(new Ser())));
$x = [1];
$y = ["a" => &$x, "b" => &$x];
echo serialize($y), "\n";
$obj = new stdClass();
$obj->self = $obj;
echo serialize($obj), "\n";
var_dump(unserialize('b:1;'), unserialize('i:42;'), unserialize('d:0.1;'));
echo serialize(0.1 + 0.2), " ", serialize(1.0), " ", serialize(-0.0), "\n";
var_dump(unserialize('garbage'));
var_dump(unserialize('O:8:"NoSuchCl":0:{}'));
var_dump(unserialize('a:1:{i:0;O:3:"Ser":0:{}}', ['allowed_classes' => false]));
