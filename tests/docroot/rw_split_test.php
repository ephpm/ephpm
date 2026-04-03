<?php
/**
 * Read/write split e2e test helper.
 *
 * Exercises both read (SELECT) and write (INSERT) paths through the DB
 * proxy when R/W splitting is enabled. In single-backend mode (no
 * replicas), all queries fall back to the primary.
 *
 * Usage: GET /rw_split_test.php?action=<action>
 *   - setup:   CREATE TABLE + INSERT seed row + SELECT it back
 *   - read:    SELECT all rows
 *   - write:   INSERT a row (pass &value=<value>)
 *   - mixed:   INSERT then immediately SELECT (tests sticky routing)
 *   - cleanup: DROP TABLE
 */

header('Content-Type: application/json');

$host = getenv('DB_HOST') ?: '127.0.0.1';
$port = getenv('DB_PORT') ?: '3306';
$action = $_GET['action'] ?? 'read';

try {
    $pdo = new PDO(
        "mysql:host={$host};port={$port}",
        'root',
        '',
        [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]
    );

    switch ($action) {
        case 'setup':
            // Write: create table and insert seed row
            $pdo->exec('CREATE TABLE IF NOT EXISTS rw_test (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL, created_at TEXT DEFAULT CURRENT_TIMESTAMP)');
            $pdo->exec("INSERT INTO rw_test (value) VALUES ('seed')");

            // Read: verify the seed row exists
            $stmt = $pdo->query('SELECT id, value FROM rw_test ORDER BY id');
            $rows = $stmt->fetchAll(PDO::FETCH_ASSOC);
            echo json_encode(['status' => 'ok', 'action' => 'setup', 'rows' => $rows]);
            break;

        case 'read':
            $stmt = $pdo->query('SELECT id, value FROM rw_test ORDER BY id');
            $rows = $stmt->fetchAll(PDO::FETCH_ASSOC);
            echo json_encode(['status' => 'ok', 'action' => 'read', 'rows' => $rows]);
            break;

        case 'write':
            $value = $_GET['value'] ?? 'default';
            $stmt = $pdo->prepare('INSERT INTO rw_test (value) VALUES (?)');
            $stmt->execute([$value]);
            echo json_encode(['status' => 'ok', 'action' => 'write', 'id' => $pdo->lastInsertId()]);
            break;

        case 'mixed':
            // Write then immediately read — exercises sticky-after-write
            $value = $_GET['value'] ?? 'mixed';
            $stmt = $pdo->prepare('INSERT INTO rw_test (value) VALUES (?)');
            $stmt->execute([$value]);
            $insert_id = $pdo->lastInsertId();

            // Immediate read on the same connection
            $stmt = $pdo->query('SELECT id, value FROM rw_test ORDER BY id');
            $rows = $stmt->fetchAll(PDO::FETCH_ASSOC);
            echo json_encode([
                'status' => 'ok',
                'action' => 'mixed',
                'insert_id' => $insert_id,
                'rows' => $rows,
            ]);
            break;

        case 'cleanup':
            $pdo->exec('DROP TABLE IF EXISTS rw_test');
            echo json_encode(['status' => 'ok', 'action' => 'cleanup']);
            break;

        default:
            http_response_code(400);
            echo json_encode(['status' => 'error', 'message' => "unknown action: {$action}"]);
    }
} catch (PDOException $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'message' => $e->getMessage()]);
}
