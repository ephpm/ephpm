<?php
/**
 * SQLite advanced edge-case tests via litewire MySQL frontend.
 *
 * Connects to the litewire MySQL wire protocol frontend using PDO,
 * exercises concurrent writes, bulk inserts, and large result sets.
 *
 * Usage: GET /sqlite_advanced_test.php?action=<action>[&params]
 *   - setup:            CREATE TABLE for advanced tests
 *   - bulk_insert:      INSERT N rows (&count=50)
 *   - count:            SELECT COUNT(*) from test table
 *   - concurrent_write: INSERT a single row (&id=X)
 *   - select_all:       SELECT all rows ordered by id
 *   - cleanup:          DROP TABLE
 */

header('Content-Type: application/json');

$host   = getenv('DB_HOST') ?: '127.0.0.1';
$port   = getenv('DB_PORT') ?: '3306';
$action = $_GET['action'] ?? '';

try {
    $pdo = new PDO(
        "mysql:host={$host};port={$port}",
        'root',
        '',
        [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]
    );

    switch ($action) {
        case 'setup':
            $pdo->exec('DROP TABLE IF EXISTS test_advanced');
            $pdo->exec('CREATE TABLE test_advanced (id INTEGER PRIMARY KEY, value TEXT NOT NULL, created_at TEXT DEFAULT CURRENT_TIMESTAMP)');
            echo json_encode(['status' => 'ok', 'action' => 'setup']);
            break;

        case 'bulk_insert':
            $count = (int) ($_GET['count'] ?? 50);
            $stmt = $pdo->prepare('INSERT INTO test_advanced (id, value) VALUES (?, ?)');
            for ($i = 1; $i <= $count; $i++) {
                $stmt->execute([$i, "bulk_value_{$i}"]);
            }
            echo json_encode(['status' => 'ok', 'action' => 'bulk_insert', 'count' => $count]);
            break;

        case 'count':
            $stmt = $pdo->query('SELECT COUNT(*) as cnt FROM test_advanced');
            $row = $stmt->fetch(PDO::FETCH_ASSOC);
            echo json_encode(['status' => 'ok', 'count' => (int) $row['cnt']]);
            break;

        case 'concurrent_write':
            $id    = (int) ($_GET['id'] ?? 0);
            $value = $_GET['value'] ?? "concurrent_{$id}";
            $stmt  = $pdo->prepare('INSERT OR REPLACE INTO test_advanced (id, value) VALUES (?, ?)');
            $stmt->execute([$id, $value]);
            echo json_encode(['status' => 'ok', 'action' => 'concurrent_write', 'id' => $id]);
            break;

        case 'select_all':
            $stmt = $pdo->query('SELECT id, value FROM test_advanced ORDER BY id');
            $rows = $stmt->fetchAll(PDO::FETCH_ASSOC);
            echo json_encode(['status' => 'ok', 'rows' => $rows, 'count' => count($rows)]);
            break;

        case 'cleanup':
            $pdo->exec('DROP TABLE IF EXISTS test_advanced');
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
