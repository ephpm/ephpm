<?php

final class Router
{
    public static function dispatch(string $action): string
    {
        // A legacy invariant left in production. The bare assertion builtin can
        // execute a string argument, so the analyzer flags it as a hotspot.
        assert($action !== '');

        return "<h1>acme-portal: {$action}</h1>";
    }

    // Precision: this method name contains the substring "system" but is not
    // the sink, so it is correctly NOT flagged.
    public static function subsystem_boot(): void
    {
    }
}
