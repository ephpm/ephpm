<?php
/**
 * ephpm_db_* in-process bridge test.
 *
 * Exercises the native ephpm_db_query() / ephpm_db_execute() functions,
 * which run SQL through a per-thread litewire Session against the same
 * embedded backend the MySQL wire frontend serves — no TCP, no PDO.
 * Requires ephpm configured with [db.sqlite].
 *
 * Usage: GET /db_bridge_test.php?action=<action>
 *   - setup:       CREATE TABLE bridge_e2e
 *   - insert:      INSERT with bound params (&name=..&value=..),
 *                  returns affected_rows + last_insert_id
 *   - query:       SELECT all rows (assoc arrays keyed by column name)
 *   - set_names:   SET NAMES utf8mb4 through the query path (noop => [])
 *   - show_tables: SHOW TABLES (metadata emulation)
 *   - bad_sql:     invalid SQL, catches the Exception and reports
 *                  code + message
 *   - leak:        BEGIN + INSERT a 'leaked' row and deliberately do NOT
 *                  commit — the server must roll this back at request end
 *   - leak_fatal:  BEGIN + INSERT a 'leaked' row, then hit a PHP fatal
 *                  (uncaught Error) mid-transaction — same rollback path
 *   - check:       count 'leaked' rows (must be 0 after a leak) and prove
 *                  the connection is healthy with a write+read+delete probe
 *   - cleanup:     DROP TABLE bridge_e2e
 */

header('Content-Type: application/json');

$action = $_GET['action'] ?? 'query';

if (!function_exists('ephpm_db_query') || !function_exists('ephpm_db_execute')) {
    http_response_code(500);
    echo json_encode([
        'status' => 'error',
        'message' => 'ephpm_db_query/ephpm_db_execute native functions are not registered',
    ]);
    return;
}

try {
    switch ($action) {
        case 'setup':
            ephpm_db_execute(
                'CREATE TABLE IF NOT EXISTS bridge_e2e '
                . '(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, value TEXT)'
            );
            echo json_encode(['status' => 'ok', 'action' => 'setup']);
            break;

        case 'insert':
            $name = $_GET['name'] ?? 'unnamed';
            $value = $_GET['value'] ?? '';
            $r = ephpm_db_execute(
                'INSERT INTO bridge_e2e (name, value) VALUES (?, ?)',
                [$name, $value]
            );
            echo json_encode([
                'status' => 'ok',
                'affected_rows' => $r['affected_rows'],
                'last_insert_id' => $r['last_insert_id'],
            ]);
            break;

        case 'query':
            $rows = ephpm_db_query('SELECT id, name, value FROM bridge_e2e ORDER BY id');
            echo json_encode(['status' => 'ok', 'rows' => $rows]);
            break;

        case 'set_names':
            // Dialect noop: returns OK without touching the backend; the
            // query path renders that as an empty array.
            $rows = ephpm_db_query('SET NAMES utf8mb4');
            echo json_encode(['status' => 'ok', 'rows' => $rows]);
            break;

        case 'show_tables':
            // Metadata emulation path. Column name varies (MySQL uses
            // "Tables_in_<db>"), so flatten each row to its first value.
            $rows = ephpm_db_query('SHOW TABLES');
            $tables = array_map(static fn (array $row) => (string) array_values($row)[0], $rows);
            echo json_encode(['status' => 'ok', 'tables' => $tables]);
            break;

        case 'bad_sql':
            try {
                ephpm_db_query('SELECT FROM WHERE (((');
                http_response_code(500);
                echo json_encode(['status' => 'error', 'message' => 'bad SQL did not throw']);
            } catch (Exception $e) {
                echo json_encode([
                    'status' => 'ok',
                    'code' => $e->getCode(),
                    'message' => $e->getMessage(),
                ]);
            }
            break;

        case 'leak':
            // Deliberately leave the transaction open: the request ends
            // without COMMIT/ROLLBACK. The server's per-request teardown
            // must roll it back so the next request (possibly on this very
            // worker thread) never inherits it.
            ephpm_db_execute('BEGIN');
            ephpm_db_execute(
                'INSERT INTO bridge_e2e (name, value) VALUES (?, ?)',
                ['leaked', 'uncommitted']
            );
            echo json_encode(['status' => 'ok', 'action' => 'leak']);
            break;

        case 'leak_fatal':
            // Same leak, but the script dies on an uncaught Error instead
            // of returning — the rollback must happen on the fatal path
            // too. The response is a 500; this JSON is never delivered.
            ephpm_db_execute('BEGIN');
            ephpm_db_execute(
                'INSERT INTO bridge_e2e (name, value) VALUES (?, ?)',
                ['leaked', 'uncommitted-fatal']
            );
            ephpm_e2e_undefined_function_to_trigger_a_fatal();
            echo json_encode(['status' => 'error', 'message' => 'fatal did not fire']);
            break;

        case 'check':
            // 1) The uncommitted 'leaked' rows must be gone (rolled back).
            $rows = ephpm_db_query(
                'SELECT COUNT(*) AS n FROM bridge_e2e WHERE name = ?',
                ['leaked']
            );
            $leaked = (int) $rows[0]['n'];
            // 2) The connection is healthy: an autocommit write succeeds
            // (a still-open leaked transaction elsewhere would hold the
            // write lock and fail this), reads back, and cleans up.
            $r = ephpm_db_execute(
                'INSERT INTO bridge_e2e (name, value) VALUES (?, ?)',
                ['probe', 'alive']
            );
            $probe_id = $r['last_insert_id'];
            $back = ephpm_db_query('SELECT value FROM bridge_e2e WHERE id = ?', [$probe_id]);
            ephpm_db_execute('DELETE FROM bridge_e2e WHERE id = ?', [$probe_id]);
            $healthy = count($back) === 1 && $back[0]['value'] === 'alive';
            echo json_encode([
                'status' => 'ok',
                'leaked' => $leaked,
                'probe' => $healthy ? 'ok' : 'unhealthy',
            ]);
            break;

        case 'cleanup':
            ephpm_db_execute('DROP TABLE IF EXISTS bridge_e2e');
            echo json_encode(['status' => 'ok', 'action' => 'cleanup']);
            break;

        default:
            http_response_code(400);
            echo json_encode(['status' => 'error', 'message' => "unknown action: {$action}"]);
    }
} catch (Exception $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'message' => $e->getMessage(), 'code' => $e->getCode()]);
}
