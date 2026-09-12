<?php
// acme-portal — front controller.
// DELIBERATELY-INSECURE demo app for `ephpm analyze`. Do not copy this code.

require __DIR__ . '/../src/Router.php';

$action = $_GET['action'] ?? 'home';

// A "diagnostics" endpoint that shells out with unsanitized request input.
// Command injection: /?action=ping&host=8.8.8.8;rm+-rf+/
if ($action === 'ping') {
    $host = $_GET['host'];
    system('ping -c 1 ' . $host);
}

// A "debug console" that runs the request body as code — a planted-webshell shape.
if ($action === 'debug') {
    eval($_POST['expr']);
}

echo Router::dispatch($action);
