<?php
class Visib {
    public int $pub = 1;
    protected string $prot = "p";
    private array $priv = [1];
    public $untyped;
    public ?self $nil = null;
}
var_dump(new stdClass());
$o = new stdClass();
$o->x = 1;
$o->{"weird key"} = [true];
var_dump($o);
var_dump(new Visib());
$c = new Visib();
$c->dyn = 3.5; // dynamic property: deprecated on non-AllowDynamicProperties classes
var_dump($c);
