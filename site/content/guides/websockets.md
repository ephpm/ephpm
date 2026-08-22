+++
title = "WebSockets"
weight = 18
+++

> **Experimental.** Native WebSocket support is opt-in (`[server.websocket] enabled = true`, off by default) and the API may change before it stabilizes. Everything on this page is implemented and covered by tests; nothing here is aspirational.

ePHPm terminates WebSockets in **Rust** and invokes PHP **per event**. PHP never holds a connection open.

That is the AWS API Gateway / RoadRunner model, not the Swoole model:

| | ePHPm | Swoole / ReactPHP |
|---|---|---|
| Who owns the socket | the Rust reactor | a PHP process |
| Cost of an idle connection | a registry entry + reactor memory | a PHP coroutine or process |
| What runs per message | one ordinary PHP request | a userland event-loop callback |
| Code shape | stateless handler, like `index.php` | long-lived event loop |

Ten thousand idle connections cost no PHP at all. A message costs exactly one PHP execution, on the same path an HTTP request takes — so `open_basedir`, the per-vhost temp and session directories, per-site database credentials, OPcache, and the crash guard all apply unchanged.

## Enabling it

```toml
[server.websocket]
enabled = true
```

That is the only required line; every bound has a default (see [Configuration](/reference/config/#server-websocket)).

The entrypoint is `websocket.php` at the **vhost's document root** — `index.php` for WebSockets. The name is configurable, `index_files`-style:

```toml
[server]
websocket_files = ["websocket.php"]   # default; tried in order
```

**A vhost with no entrypoint 404s every upgrade request.** It never falls through to `index.php`, a static file, or the `[server] fallback` chain. Opting a vhost into WebSockets means putting the file there.

## The three events

One script handles all of them. Userland routes on `$_SERVER['WS_EVENT']` — that is the point; there is no framework contract to satisfy.

| `WS_EVENT` | When | Notes |
|---|---|---|
| `connect` | **Before** the handshake completes | Return 2xx to accept the upgrade, anything else to refuse it. **Authentication belongs here** — it is the only point at which an upgrade can still be refused. |
| `message` | One inbound data frame | Payload is the request body (`php://input`). `WS_OPCODE` is `text` or `binary`. |
| `disconnect` | After the socket closes | **Best-effort**, like API Gateway's `$disconnect`. Not delivered if the process dies. |

Every event also gets:

| `$_SERVER` key | Value |
|---|---|
| `WS_EVENT` | `connect`, `message`, or `disconnect` |
| `WS_CONNECTION_ID` | 32 lowercase hex characters — 128 random bits |
| `WS_OPCODE` | `text` or `binary`. **Only on `message` events** |

The upgrade request's URI, query string, headers and cookies are replayed on every event, so `$_GET`, `$_COOKIE` and `$_SERVER['HTTP_*']` look the same in a `message` handler as they did at `connect`.

Because `WS_EVENT` is only set for WebSocket events, an ordinary HTTP request can tell the difference with `isset($_SERVER['WS_EVENT'])`.

## A complete minimal `websocket.php`

```php
<?php
// public/websocket.php — a working echo + chat server.

$event = $_SERVER['WS_EVENT'];
$conn  = $_SERVER['WS_CONNECTION_ID'];

switch ($event) {
    case 'connect':
        // Authenticate. A non-2xx response refuses the upgrade and the
        // client sees this exact status and body.
        $token = $_GET['token'] ?? '';
        $userId = verify_token($token);          // your app's function
        if ($userId === null) {
            http_response_code(401);
            header('WWW-Authenticate: Bearer');
            echo 'unauthorized';
            return;
        }

        // Remember who this socket belongs to, so an ordinary HTTP request
        // can push to it later. See "Pushing from an HTTP request" below.
        ephpm_kv_set("ws:user:$userId", $conn);
        ephpm_kv_set("ws:conn:$conn", (string) $userId);

        // Put them in a room. Channels are how you fan out.
        ephpm_ws_subscribe('lobby');

        http_response_code(200);
        return;

    case 'message':
        $payload = json_decode(file_get_contents('php://input'), true);

        match ($payload['type'] ?? '') {
            // Reply to just this socket. No connection id needed — the
            // implicit form acts on the connection that fired this event.
            'ping' => ephpm_ws_send(json_encode(['type' => 'pong'])),

            // Fan out to everyone in the room, including this socket.
            'chat' => ephpm_ws_broadcast('lobby', json_encode([
                'type' => 'chat',
                'from' => ephpm_kv_get("ws:conn:$conn"),
                'text' => $payload['text'] ?? '',
            ])),

            default => ephpm_ws_send(json_encode(['type' => 'error'])),
        };
        return;

    case 'disconnect':
        // Best-effort cleanup.
        $userId = ephpm_kv_get("ws:conn:$conn");
        if ($userId !== null) {
            ephpm_kv_del("ws:user:$userId");
            ephpm_kv_del("ws:conn:$conn");
        }
        return;
}
```

Client side, nothing special:

```js
const ws = new WebSocket(`wss://example.com/socket?token=${token}`);
ws.onmessage = (e) => console.log(JSON.parse(e.data));
ws.onopen = () => ws.send(JSON.stringify({ type: 'chat', text: 'hello' }));
```

The path (`/socket`) is not routed — **any** upgrade request to the vhost runs `websocket.php`. Use the path or query string however you like; your script sees both.

## Pushing from an HTTP request

This is the feature that makes the model worth it: **any** PHP execution can push to a live socket, not just a WebSocket event. A normal `POST /comments` handler can notify everyone watching that post.

The pattern has two halves. During `connect`, record the connection id under an identity your app already knows:

```php
// in websocket.php, connect branch
ephpm_kv_set("ws:user:$userId", $_SERVER['WS_CONNECTION_ID']);
```

Later, from an ordinary request, look it up and send:

```php
<?php
// POST /comments — an entirely normal HTTP handler.
$comment = create_comment($_POST['post_id'], $_POST['body']);

// Push to one specific user, if they happen to be connected.
$connId = ephpm_kv_get("ws:user:{$comment['author_id']}");
if ($connId !== null) {
    ephpm_ws_connection_send($connId, json_encode([
        'type' => 'comment.created',
        'id'   => $comment['id'],
    ]));
}

// Or push to everyone watching this post — no id lookup needed.
$sent = ephpm_ws_broadcast("post:{$comment['post_id']}", json_encode([
    'type' => 'comment.created',
    'body' => $comment['body'],
]));

header('Content-Type: application/json');
echo json_encode(['ok' => true, 'notified' => $sent]);
```

Note the two different functions. `ephpm_ws_send()` is the **implicit** form: it means "the connection that fired the current event", so it only works inside a WebSocket event. From an HTTP request there is no current connection, and calling it **throws** rather than silently doing nothing. Use `ephpm_ws_connection_send()` with an explicit id, or `ephpm_ws_broadcast()`, which needs no connection at all.

`ephpm_kv_*` is the natural place to keep the id→identity mapping — it is in-process, so the lookup is a hash-map read, and in a cluster the mapping gossips to every node. Any store works; a database column is fine too.

## Function reference

Each operation has an **implicit** form (acts on the current event's connection) and an **explicit** form (`ephpm_ws_connection_*`, takes a connection id). `ephpm_ws_broadcast()` has only one form because a channel is not a connection.

| Function | Returns | Notes |
|---|---|---|
| `ephpm_ws_send(string $payload, bool $binary = false)` | `bool` | Push to the current event's connection. Throws outside a WebSocket event. |
| `ephpm_ws_connection_send(string $connection_id, string $payload, bool $binary = false)` | `bool` | Push to any connection in this site. |
| `ephpm_ws_subscribe(string $channel)` | `bool` | Join the current connection to a channel. |
| `ephpm_ws_connection_subscribe(string $connection_id, string $channel)` | `bool` | |
| `ephpm_ws_unsubscribe(string $channel)` | `bool` | |
| `ephpm_ws_connection_unsubscribe(string $connection_id, string $channel)` | `bool` | |
| `ephpm_ws_broadcast(string $channel, string $payload, bool $binary = false)` | `int` | Number of connections the frame was **queued** to. Works from any PHP execution. |
| `ephpm_ws_close(int $code = 1000)` | `bool` | Close the current connection. Throws outside a WebSocket event. |
| `ephpm_ws_connection_close(string $connection_id, int $code = 1000)` | `bool` | |

**`false` versus an exception.** A `false` return means the connection could not be reached — it is unknown to this site, it has gone, or its outbound queue is full. Those are ordinary runtime outcomes; check the return value if you care. An **exception** means the call could never have worked: WebSockets are disabled server-wide, the request has no virtual host, or the implicit form was used with no current connection. Those are bugs or misconfiguration, so they are not silently swallowed.

Closing is asynchronous. `ephpm_ws_close()` asks the connection's session task to close, so frames already queued ahead of it are still delivered.

## Multi-tenant isolation

Connection ids and channel names are **scoped to a virtual host**. A connection opened on `a.example.com` cannot be reached from PHP running for `b.example.com`, even with the exact connection id in hand — and `chat` on one vhost is a different channel from `chat` on another.

The scope is not taken from a function argument. It comes from the canonical site key the router derived for the request that is executing, the same key that selects the per-site database and KV keyspace. A request whose `Host` matches no virtual host has no tenant identity, so it gets no WebSocket capability at all: its upgrade requests 404 and its `ephpm_ws_*` calls throw.

Connection ids are 128 random bits from the OS CSPRNG, rendered as 32 hex characters. Treat one as a capability: anything holding it, within the owning site, can push to that socket.

## Limits and back-pressure

Every connection has a **bounded** outbound queue (`[server.websocket] send_queue`, 64 frames by default). When it fills — a client that has stopped reading — the frame is **not** buffered. `ephpm_ws_send()` returns `false`, the broadcast does not count that subscriber, and the socket is closed with WebSocket status `1013 Try Again Later`. One slow reader costs one connection, never the server's memory.

The other bounds:

| Knob | Default | What it protects |
|---|---|---|
| `max_connections` | `10000` | Total sockets, all vhosts. Over the cap, upgrades get `503`. |
| `max_connections_per_site` | `1000` | One tenant cannot consume a shared node's whole budget. |
| `max_message_size` | 1 MiB | Largest inbound message, and largest payload PHP may push. |
| `max_frame_size` | 1 MiB | Largest single inbound frame. |
| `ping_interval_secs` | `30` | Server-initiated keepalive. |
| `idle_timeout_secs` | `120` | Close a connection that has received nothing (not even a pong) for this long. |

A connection is also subject to a fixed ceiling of 64 channel subscriptions and 256-byte channel names. These are not configurable — they bound what one misbehaving script can pin, and no deployment has a reason to tune them.

A `message` handler that exceeds `[server.timeouts] request` closes its connection with `1011`. Unlike an HTTP request, the socket cannot simply be abandoned: the blocking PHP execution cannot be cancelled, so continuing would risk a second dispatch overlapping the abandoned one.

## Metrics

With `[server.metrics] enabled = true`, the WebSocket series appear at `/metrics` — see [Metrics](/reference/metrics/#websockets). The two most worth alerting on:

- `ephpm_ws_send_queue_overflow_total` rising means clients are not draining, and connections are being shed.
- `ephpm_ws_cross_site_denied_total` should be flat at zero. Anything else is a bug or an attempt.

## Limitations

- **HTTP/1.1 only.** WebSocket upgrades cannot be expressed on the HTTP/2 or HTTP/3 request paths ePHPm serves (RFC 8441 extended CONNECT is not implemented). Browsers open a dedicated HTTP/1.1 connection for `ws:`/`wss:` regardless, so this is not something clients encounter. TLS works normally — `wss://` is served by the same listener as `https://`.
- **Not supported in worker mode.** `[server.websocket] enabled = true` with `[php] mode = "worker"` is a startup error: worker mode routes every request into the persistent worker's loop, so the entrypoint would never execute. Use fpm mode (the default).
- **`disconnect` is best-effort.** A process that dies takes its in-flight `disconnect` events with it. Do not rely on it for anything that must happen exactly once; treat it as a cleanup hint, and expire the state you write at `connect`.
- **No clustered fan-out yet.** The registry is per-process, so `ephpm_ws_broadcast()` reaches connections on **this node** only. In a multi-node cluster, a socket connected to node A is not reachable from a request served by node B. Cluster-wide fan-out is not implemented.
- **Do not push from a shutdown function or a destructor.** ePHPm keeps one long-lived SAPI request open per worker thread, so `register_shutdown_function()` callbacks and `__destruct()` on shutdown run when the *thread* retires, not at the end of your request — and the thread's site scope is gone by then. `ephpm_ws_*` calls made there fail closed (no capability) rather than pushing to whatever socket happened to run last on that thread. Push while the request is still live.
- **`[server.security] allowed_php_paths` does not gate the entrypoint.** The entrypoint is selected by an operator-configured filename, not by the request path, so there is no client-driven path to allowlist. Entries are still containment-checked against the document root.
- **Graceful shutdown does not drain WebSockets.** An upgraded socket is handed to its own task and stops counting against `[server.limits] max_connections`; shutdown does not wait for it. Clients should reconnect.
