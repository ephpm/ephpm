<?php

/**
 * Worker-mode E2E fixture script (document_root: tests/worker-docroot).
 *
 * Serves the reference contract the `worker_mode` e2e suite asserts against:
 *
 *   hello <REQUEST_URI> (boot #<B>, request #<R>)
 *
 * plus three trigger routes the suite drives:
 *
 *   ?__fatal=1     an uncatchable fatal mid-request (bailout -> 500 + recycle)
 *   ?__bailout=1   output, then a real zend_bailout (memory exhaustion), with
 *                  the response never sent (-> 500 + recycle, no partial body)
 *   ?__exit=1      output followed by exit() mid-request (exit synthesis)
 *   ?__blocks=<n>  a WordPress-`do_blocks()`-shaped render workload (issue #116)
 *   ?__deep=<n>    the same shape taken past the C-stack ceiling (issue #116);
 *                  add &catch=1 to handle the resulting Error in userland
 *
 * This is a fixture, not a framework adapter — the shipped reference script is
 * `examples/worker/worker.php`. It is deliberately kept close to that one so a
 * divergence between the two is obvious.
 */

declare(strict_types=1);

// ── Boot-once section ────────────────────────────────────────────────
static $bootCount = 0;
$bootCount++;
$myBoot = $bootCount;
$requestCount = 0;

error_log("[worker] booted (boot #{$myBoot})");

/**
 * Render nested markup the way WordPress' block renderer does, so the engine
 * paths issue #116 implicated are all on the request hot path:
 *
 *  - userland recursion that re-enters through *internal* functions
 *    (`preg_replace_callback`, `array_map`) — the C-stack re-entry shape that
 *    `do_blocks()` -> `render_block()` -> `apply_filters()` produces;
 *  - a nested userland output buffer per nesting level, opened and closed
 *    inside the recursion (worker mode has no per-request RSHUTDOWN to clean
 *    these up, which is why the reset path has to);
 *  - a response large enough to force the SAPI capture buffer to grow through
 *    several reallocs on every request.
 *
 * Deterministic: the same $depth always produces byte-identical markup, so the
 * suite can assert render fidelity across requests on a persistent worker.
 */
function ephpm_e2e_render_blocks(int $depth, int $breadth): string
{
    if ($depth <= 0) {
        return '<p>leaf</p>';
    }

    ob_start();
    for ($i = 0; $i < $breadth; $i++) {
        $inner = ephpm_e2e_render_blocks($depth - 1, $breadth);
        // Internal function -> userland callback: the same re-entry the block
        // renderer performs through the filter chain.
        $inner = (string) preg_replace_callback(
            '/<p>([a-z]+)<\/p>/',
            static fn (array $m): string => '<p class="d' . $depth . '">' . strrev($m[1]) . '</p>',
            $inner
        );
        echo '<div class="lvl-', $depth, '-', $i, '">', $inner, '</div>';
    }
    $level = (string) ob_get_clean();

    return implode('', array_map(static fn (string $s): string => $s, [$level]));
}

/**
 * The same nesting shape as ephpm_e2e_render_blocks(), stripped to the one
 * thing that consumes C stack — a userland recursion that re-enters through an
 * *internal* function — and driven past the thread's stack ceiling (issue #116).
 *
 * PHP 8.3+ tests `zend_call_stack_overflowed(EG(stack_limit))` at exactly these
 * re-entry points and raises a catchable
 * `Error: Maximum call stack size ... reached`. ePHPm used to override
 * `zend_call_stack_init()` with a no-op on Linux, so `EG(stack_limit)` stayed
 * NULL, the checkpoint never fired, and this walked off the end of the worker
 * thread's stack: SIGSEGV, whole process gone — not just the worker.
 */
function ephpm_e2e_deep_render(int $level): string
{
    if ($level <= 0) {
        return '<p>leaf</p>';
    }

    return array_map(
        static fn (int $ignored): string => '<div>' . ephpm_e2e_deep_render($level - 1) . '</div>',
        [1],
    )[0];
}

/**
 * A stream whose SECOND read dies in a Zend bailout.
 *
 * `\Ephpm\Worker\send_response_stream()` delivers status + headers first, then
 * pumps this stream chunk by chunk. Faulting on the second read therefore puts
 * the bailout *after* the response line is already on the wire — the one case
 * where a 500 is no longer available, and ePHPm has to break the body framing
 * instead so the client's transfer fails rather than completing.
 */
class EphpmE2eBailoutStream
{
    /** @var resource|null set by the stream layer */
    public $context;

    private int $reads = 0;

    public function stream_open(string $path, string $mode, int $options, ?string &$openedPath): bool
    {
        return true;
    }

    public function stream_read(int $count): string
    {
        $this->reads++;
        if ($this->reads === 1) {
            return "STREAM-CHUNK-BEFORE-BAILOUT\n";
        }
        // Blow the memory limit from inside php_stream_read(), i.e. from
        // inside the C pump loop of send_response_stream().
        ini_set('memory_limit', '2M');
        $chunks = [];
        for ($i = 0; $i < 100; $i++) {
            $chunks[] = str_repeat('x', 1024 * 1024);
        }

        return "STREAM-CHUNK-UNREACHABLE\n";
    }

    public function stream_eof(): bool
    {
        return false;
    }

    public function stream_stat(): array
    {
        return [];
    }
}

// ── Request loop ─────────────────────────────────────────────────────
while (($envelope = \Ephpm\Worker\take_request()) !== null) {
    $requestCount++;

    $server = $envelope->serverVars();
    $uri = $server['REQUEST_URI'] ?? '/';
    $query = $envelope->query();

    // Fatal trigger: an uncatchable fatal must 500 the in-flight request and
    // recycle this worker without wedging the pool.
    if (isset($query['__fatal'])) {
        ephpm_e2e_this_function_does_not_exist(); // intentional fatal
    }

    // Bailout trigger: echo a marker, then blow the memory limit. That is a
    // real zend_bailout, absorbed by php_execute_script's own zend_try, so the
    // worker loop simply "returns" with this request still in flight and the
    // capture buffers holding a truncated prefix. The engine must refuse to
    // synthesize a response from them — the client gets a 500, never the
    // marker.
    if (isset($query['__bailout'])) {
        header('Content-Type: text/plain; charset=utf-8');
        echo "WORKER-BAILOUT-PARTIAL-OUTPUT\n";
        ini_set('memory_limit', '2M');
        $chunks = [];
        for ($i = 0; $i < 100; $i++) {
            $chunks[] = str_repeat('x', 1024 * 1024);
        }
        echo "WORKER-BAILOUT-UNREACHABLE\n";
    }

    // Streaming-bailout trigger: status + headers go out first, then the pump
    // dies mid-body. There is no 500 left to send, so the body must NOT be
    // terminated cleanly — the client has to see a broken transfer.
    if (isset($query['__stream_bailout'])) {
        if (!in_array('ephpmbail', stream_get_wrappers(), true)) {
            stream_wrapper_register('ephpmbail', EphpmE2eBailoutStream::class);
        }
        $fh = fopen('ephpmbail://go', 'r');
        \Ephpm\Worker\send_response_stream(
            200,
            ['Content-Type' => 'text/plain; charset=utf-8'],
            $fh
        );
        continue;
    }

    // exit() trigger: output already echoed plus output still sitting in a
    // userland buffer must both reach the client (engine-side exit synthesis
    // flushes the buffers), and the server must keep serving afterwards.
    if (isset($query['__exit'])) {
        header('Content-Type: text/plain; charset=utf-8');
        echo "exit-route echoed\n";
        ob_start();
        echo "exit-route buffered\n";
        exit;
    }

    // Runaway-recursion trigger (issue #116). Two shapes:
    //
    //   ?__deep=<n>            let the Error escape — the engine must turn it
    //                          into a 500 and recycle this worker, and the POOL
    //                          must keep serving (before the fix the process
    //                          died, so there was no pool left).
    //   ?__deep=<n>&catch=1    catch it in userland — proof that the overflow
    //                          now arrives as an ordinary catchable Error, so
    //                          this worker answers 200 and stays booted.
    if (isset($query['__deep'])) {
        $levels = max(1, min(2000000, (int) $query['__deep']));
        if (isset($query['catch'])) {
            try {
                $html = ephpm_e2e_deep_render($levels);
                \Ephpm\Worker\send_response(
                    200,
                    ['Content-Type' => 'text/plain; charset=utf-8'],
                    "deep-survived levels={$levels} len=" . strlen($html)
                        . " boot={$myBoot} request={$requestCount}\n"
                );
            } catch (\Throwable $e) {
                \Ephpm\Worker\send_response(
                    200,
                    ['Content-Type' => 'text/plain; charset=utf-8'],
                    'deep-caught ' . get_class($e) . ': ' . $e->getMessage()
                        . " boot={$myBoot} request={$requestCount}\n"
                );
            }
            continue;
        }

        $html = ephpm_e2e_deep_render($levels);
        \Ephpm\Worker\send_response(
            200,
            ['Content-Type' => 'text/plain; charset=utf-8'],
            "deep-survived levels={$levels} len=" . strlen($html) . "\n"
        );
        continue;
    }

    // Block-render-shaped workload (issue #116).
    if (isset($query['__blocks'])) {
        $depth = max(1, min(5, (int) $query['__blocks']));
        $html = ephpm_e2e_render_blocks($depth, 4);
        $body = $html . sprintf(
            "\n<!-- blocks depth=%d len=%d sha1=%s boot=%d request=%d -->\n",
            $depth,
            strlen($html),
            sha1($html),
            $myBoot,
            $requestCount
        );

        \Ephpm\Worker\send_response(
            200,
            ['Content-Type' => 'text/html; charset=utf-8'],
            $body
        );
        continue;
    }

    \Ephpm\Worker\send_response(
        200,
        ['Content-Type' => 'text/plain; charset=utf-8'],
        "hello {$uri} (boot #{$myBoot}, request #{$requestCount})\n"
    );
}

error_log("[worker] loop ended (boot #{$myBoot}, served {$requestCount} requests)");
