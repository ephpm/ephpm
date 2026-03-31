<?php
/**
 * SQLite via litewire MySQL frontend test.
 *
 * Connects to the litewire MySQL wire protocol frontend using PDO,
 * creates a table, inserts data, queries it, and returns JSON results.
 *
 * Expects DB_HOST and DB_PORT environment variables (set by ephpm
 * when [db.sqlite] is configured).
 *
 * Usage: GET /sqlite_test.php?action=<action>
 *   - setup:   CREATE TABLE + INSERT test data
 *   - query:   SELECT all rows
 *   - insert:  INSERT a row (pass &name=<name>&value=<value>)
 *   - cleanup: DROP TABLE
 */

header('Content-Type: application/json');

$host = getenv('DB_HOST') ?: '127.0.0.1';
$port = getenv('DB_PORT') ?: '3306';
$action = $_GET['action'] ?? 'query';

try {
    $pdo = new PDO(
        "mysql:host={$host};port={$port}",
        'root',
        '',
        [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]
    );

    switch ($action) {
        case 'setup':
            $pdo->exec('CREATE TABLE IF NOT EXISTS test_kv (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, value TEXT)');
            $pdo->exec("INSERT INTO test_kv (name, value) VALUES ('key1', 'hello')");
            $pdo->exec("INSERT INTO test_kv (name, value) VALUES ('key2', 'world')");
            echo json_encode(['status' => 'ok', 'action' => 'setup']);
            break;

        case 'query':
            $stmt = $pdo->query('SELECT id, name, value FROM test_kv ORDER BY id');
            $rows = $stmt->fetchAll(PDO::FETCH_ASSOC);
            echo json_encode(['status' => 'ok', 'rows' => $rows]);
            break;

        case 'insert':
            $name = $_GET['name'] ?? 'unnamed';
            $value = $_GET['value'] ?? '';
            $stmt = $pdo->prepare('INSERT INTO test_kv (name, value) VALUES (?, ?)');
            $stmt->execute([$name, $value]);
            echo json_encode(['status' => 'ok', 'id' => $pdo->lastInsertId()]);
            break;

        case 'cleanup':
            $pdo->exec('DROP TABLE IF EXISTS test_kv');
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
