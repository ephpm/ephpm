<?php

/**
 * ePHPm worker-mode reference script (Phase 1).
 *
 * This is the hand-written proof of the worker-mode primitive — NOT a
 * framework adapter. It boots ONCE, then loops forever handling requests
 * without any per-request bootstrap. Framework adapters (Octane, PSR-15)
 * are Composer packages that consume the same two primitives:
 *
 *   \Ephpm\Worker\take_request(): ?\Ephpm\Worker\Envelope
 *   \Ephpm\Worker\send_response(int $status, array $headers, string $body): void
 *
 * Run it by pointing ephpm at this directory with worker mode enabled:
 *
 *   [server]
 *   document_root = "/path/to/examples/worker"
 *
 *   [php]
 *   mode = "worker"
 *   worker_script = "worker.php"
 *
 * Then `curl http://127.0.0.1:8080/anything` returns:
 *   hello /anything (boot #1, request #3)
 *
 * The "boot #N" counter increments ONCE per worker boot (proving zero
 * per-request bootstrap). The "request #N" counter increments per request
 * handled by this worker.
 */

declare(strict_types=1);

// ── Boot-once section ────────────────────────────────────────────────
// Everything above the take_request() loop runs exactly ONCE when the
// worker boots. A real framework would build its kernel/container here.
//
// The boot counter is a process-wide static (survives across requests on
// this worker because the loop below never re-runs this file). We use a
// static local so each *boot* increments it, proving boot-once: if ePHPm
// were re-bootstrapping per request, this would be 1 on every response.

static $bootCount = 0;
$bootCount++;               // increments once per worker boot
$myBoot = $bootCount;       // captured for this worker's lifetime
$requestCount = 0;

// error_log so the boot is visible in the server log — a real adapter would
// not do this; it is here to make boot-once observable. (STDERR is a CLI-SAPI
// constant and is not defined under the worker SAPI, so use error_log.)
error_log("[worker] booted (boot #{$myBoot})");

// ── Request loop ─────────────────────────────────────────────────────
// take_request() blocks until the runtime routes an HTTP request to this
// worker, or returns null on graceful shutdown / recycle — ending the loop
// cleanly so the runtime can respawn a fresh worker.

while (($envelope = \Ephpm\Worker\take_request()) !== null) {
    $requestCount++;

    $server = $envelope->serverVars();
    $uri = $server['REQUEST_URI'] ?? '/';

    $body = "hello {$uri} (boot #{$myBoot}, request #{$requestCount})\n";

    \Ephpm\Worker\send_response(
        200,
        ['Content-Type' => 'text/plain; charset=utf-8'],
        $body
    );
}

// Reaching here means the loop ended (shutdown or recycle). Returning
// hands control back to the ePHPm runtime, which frees this worker's PHP
// context and — unless draining — boots a replacement.
error_log("[worker] loop ended (boot #{$myBoot}, served {$requestCount} requests)");
