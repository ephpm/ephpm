<?php

final class Auth
{
    public function check(string $token): bool
    {
        // nosemgrep — an inline suppression. ephpm-analyze does NOT honor it;
        // it reports the attempt instead.
        return hash_equals($this->secret(), $token);
    }

    /** @phpstan-ignore-next-line reading a global secret on purpose */
    private function secret(): string
    {
        return getenv('ACME_SECRET') ?: '';
    }

    // Precision: a method named "assert_state" and a method call are not the
    // bare sink, so they are correctly NOT flagged.
    private function noop(): void
    {
        $this->assert_state();
    }

    private function assert_state(): void
    {
    }
}
