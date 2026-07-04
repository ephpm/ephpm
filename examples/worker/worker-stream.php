<?php

/**
 * ePHPm worker-mode STREAMING reference script (Phase 3).
 *
 * Proves the streaming request + response primitives without buffering the
 * whole body. It reads the incoming request body from a real php:// stream
 * (Envelope::bodyStream()) in fixed-size chunks and streams them straight back
 * to the client via \Ephpm\Worker\send_response_stream() — so a multi-GB upload
 * echoes back with FLAT worker memory (RSS does not grow with body size).
 *
 * Run it by pointing ephpm at this directory in worker mode:
 *
 *   [server]
 *   document_root = "/path/to/examples/worker"
 *
 *   [php]
 *   mode = "worker"
 *   worker_script = "worker-stream.php"
 *   worker_stream_threshold = 65536   # stream bodies >= 64 KiB
 *
 * Then stream a large body through and get the same bytes back:
 *
 *   head -c 200000000 /dev/urandom > /tmp/blob
 *   curl --data-binary @/tmp/blob http://127.0.0.1:8080/echo -o /tmp/out
 *   cmp /tmp/blob /tmp/out    # identical; worker RSS stayed flat
 *
 * The two Phase-3 primitives beyond Phase 1:
 *
 *   $envelope->bodyStream(): resource        // readable php:// request-body stream
 *   \Ephpm\Worker\send_response_stream(int $status, array $headers, $bodyResource): void
 */

declare(strict_types=1);

error_log('[worker-stream] booted');

while (($envelope = \Ephpm\Worker\take_request()) !== null) {
    // A readable php:// stream over the incremental request body. Reading it
    // pulls fixed-size chunks from ePHPm without pre-buffering the whole body.
    $in = $envelope->bodyStream();

    // A php://temp handle that spills to disk past a small memory cap, so the
    // response producer also stays flat. Here we simply pipe input -> output;
    // a real app would transform/store the stream instead.
    $out = fopen('php://temp/maxmemory:1048576', 'r+b');

    $total = 0;
    while (!feof($in)) {
        $chunk = fread($in, 65536);
        if ($chunk === false || $chunk === '') {
            break;
        }
        $total += strlen($chunk);
        fwrite($out, $chunk);
    }
    rewind($out);

    // Stream the response body back incrementally. Bytes reach the client as
    // they are produced; the worker never holds the whole body in memory.
    \Ephpm\Worker\send_response_stream(
        200,
        [
            'Content-Type' => 'application/octet-stream',
            'X-Echo-Bytes' => (string) $total,
        ],
        $out
    );

    fclose($out);
    // $in is closed by ePHPm when the request completes.
}

error_log('[worker-stream] loop ended');
