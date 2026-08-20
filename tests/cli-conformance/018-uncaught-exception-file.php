<?php
function thrower()
{
    throw new RuntimeException("inner cause");
}
try {
    thrower();
} catch (RuntimeException $e) {
    throw new LogicException("outer", 42, $e);
}
