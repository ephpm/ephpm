<?php
trait Greets
{
    public function greet(): string
    {
        return "hi from " . static::class;
    }
}
abstract class Base
{
    abstract public function id(): int;
    public function __toString(): string
    {
        return "Base#" . $this->id();
    }
}
interface HasName
{
    public function name(): string;
}
final class Impl extends Base implements HasName
{
    use Greets;
    public function id(): int
    {
        return 7;
    }
    public function name(): string
    {
        return "impl";
    }
    public function __get($p)
    {
        return "magic-get:$p";
    }
    public function __set($p, $v)
    {
        echo "magic-set:$p=$v\n";
    }
    public function __call($m, $a)
    {
        return "call:$m(" . implode(",", $a) . ")";
    }
    public static function __callStatic($m, $a)
    {
        return "static:$m";
    }
    public function __isset($p)
    {
        return $p === "yes";
    }
}
$i = new Impl();
echo $i->greet(), "\n", (string)$i, "\n", $i->undefined_prop, "\n";
$i->something = 1;
echo $i->missingMethod(1, 2), "\n", Impl::missingStatic(), "\n";
var_dump(isset($i->yes), isset($i->no));
var_dump($i instanceof Base, $i instanceof HasName, is_subclass_of($i, Base::class));
var_dump(get_class($i), get_parent_class($i), class_implements($i), class_uses($i));
var_dump(method_exists($i, 'greet'), property_exists($i, 'nope'));
echo json_encode(get_object_vars(new class {
    public $x = 1;
    protected $y = 2;
})), "\n";
$anon = new class extends Base {
    public function id(): int
    {
        return 1;
    }
};
echo get_class($anon) === (new ReflectionClass($anon))->getName() ? "anon-ok" : "anon-mismatch", "\n";
