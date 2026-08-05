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
 *   ?__exit=1      output followed by exit() mid-request (exit synthesis)
 *   ?__blocks=<n>  a WordPress-`do_blocks()`-shaped render workload (issue #116)
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
