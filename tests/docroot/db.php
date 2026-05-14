<?php
/**
 * Generic SQL execution endpoint for hrana / sqlite e2e tests.
 *
 * Connects to litewire's MySQL frontend via PDO and executes whatever
 * SQL the client passes via ?sql=. Returns a JSON envelope so the
 * caller can distinguish success/failure shapes without parsing
 * arbitrary error text.
 *
 * Expects DB_HOST and DB_PORT (set by ephpm when [db.sqlite] is
 * configured); falls back to the in-process default proxy listener.
 *
 * Usage:
 *   GET /db.php?sql=CREATE+TABLE+...
 *   GET /db.php?sql=SELECT+...
 */

header('Content-Type: application/json');

$host = getenv('DB_HOST') ?: '127.0.0.1';
$port = getenv('DB_PORT') ?: '3306';
$sql  = $_GET['sql'] ?? '';

if ($sql === '') {
    http_response_code(400);
    echo json_encode(['status' => 'error', 'message' => 'missing ?sql=']);
    exit;
}

try {
    $pdo = new PDO(
        "mysql:host={$host};port={$port}",
        'root',
        '',
        [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]
    );

    $trimmed = strtoupper(ltrim($sql));
    if (str_starts_with($trimmed, 'SELECT') || str_starts_with($trimmed, 'PRAGMA')) {
        $stmt = $pdo->query($sql);
        $rows = $stmt->fetchAll(PDO::FETCH_ASSOC);
        echo json_encode(['status' => 'ok', 'rows' => $rows]);
    } else {
        $affected = $pdo->exec($sql);
        echo json_encode(['status' => 'ok', 'affected' => $affected]);
    }
} catch (PDOException $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'message' => $e->getMessage()]);
}
