<?php
function gen()
{
    $received = yield 1;
    echo "got:", var_export($received, true), "\n";
    yield 2;
    return "ret";
}
$g = gen();
echo $g->current(), "\n";
echo $g->send("hello"), "\n";
$g->next();
var_dump($g->valid(), $g->getReturn());
function kv()
{
    yield 'a' => 1;
    yield 'b' => 2;
}
foreach (kv() as $k => $v) {
    echo "$k=$v;";
}
echo "\n";
function inner()
{
    yield 1;
    yield 2;
}
function outer()
{
    yield 0;
    yield from inner();
    yield 3;
}
var_dump(iterator_to_array(outer(), false));
$mult = 3;
$f = fn($x) => $x * $mult;
echo $f(7), "\n";
$byval = function ($x) use ($mult) {
    return $x + $mult;
};
echo $byval(1), "\n";
$acc = 0;
$byref = function () use (&$acc) {
    $acc++;
};
$byref();
$byref();
echo $acc, "\n";
class Ctx
{
    private $secret = "s3cret";
}
$peek = Closure::bind(function () {
    return $this->secret;
}, new Ctx(), Ctx::class);
echo $peek(), "\n";
$upper = strtoupper(...);
echo $upper("abc"), "\n";
echo implode(",", array_map(fn($x) => $x ** 2, [1, 2, 3])), "\n";
