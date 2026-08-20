<?php
class D
{
    public function __construct(private string $n)
    {
    }
    public function __destruct()
    {
        echo "destruct {$this->n}\n";
    }
}
register_shutdown_function(function () {
    echo "shutdown 1\n";
});
register_shutdown_function(function () {
    echo "shutdown 2\n";
    register_shutdown_function(function () {
        echo "shutdown nested\n";
    });
});
$global = new D("global");
$a = new D("a");
$b = new D("b");
unset($a);
echo "end of script\n";
