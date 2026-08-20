<?php
try {
    throw new InvalidArgumentException("msg", 7);
} catch (Exception $e) {
    echo get_class($e), "|", $e->getMessage(), "|", $e->getCode(), "|", basename($e->getFile()), "|", $e->getLine(), "\n";
    echo $e->getTraceAsString(), "\n";
    echo $e, "\n";
}
try {
    try {
        throw new Error("E1");
    } finally {
        echo "finally ran\n";
    }
} catch (Error $e) {
    echo "caught ", $e->getMessage(), "\n";
}
function f()
{
    try {
        return "from try";
    } finally {
        echo "finally2\n";
    }
}
echo f(), "\n";
try {
    throw new TypeError("te");
} catch (Throwable $t) {
    var_dump($t instanceof Error, $t instanceof Exception, $t->getPrevious());
}
