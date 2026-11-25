<?php
/**
 * SQLite via litewire MySQL frontend test.
 *
 * Connects to the litewire MySQL wire protocol frontend using PDO,
 * creates a table, inserts data, queries it, and returns JSON results.
 *
 * Reads the DB connection parameters ephpm injects into $_SERVER when
 * [db.sqlite] is configured: DB_HOST, DB_PORT, and — now that the shared
 * MySQL wire listener authenticates by default ([db.wire_auth]) — DB_USER and
 * DB_PASSWORD. ePHPm registers these as $_SERVER entries (not into the process
 * environment), so getenv() does NOT see them; read $_SERVER first and fall
 * back only for an older binary that injected nothing.
 *
 * Usage: GET /sqlite_test.php?action=<action>
 *   - setup:   CREATE TABLE + INSERT test data
 *   - query:   SELECT all rows
 *   - insert:  INSERT a row (pass &name=<name>&value=<value>)
 *   - cleanup: DROP TABLE
 */

header('Content-Type: application/json');

function db_var(string $key, string $default): string {
    if (isset($_SERVER[$key]) && $_SERVER[$key] !== '') {
        return (string) $_SERVER[$key];
    }
    $v = getenv($key);
    return ($v !== false && $v !== '') ? $v : $default;
}

$host = db_var('DB_HOST', '127.0.0.1');
$port = db_var('DB_PORT', '3306');
$user = db_var('DB_USER', 'root');
$pass = db_var('DB_PASSWORD', '');
$action = $_GET['action'] ?? 'query';

try {
    $pdo = new PDO(
        "mysql:host={$host};port={$port}",
        $user,
        $pass,
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
