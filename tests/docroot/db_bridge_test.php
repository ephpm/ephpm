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
 *
 * v0.7.4 adapter surface (issues #259/#260/#262/#263):
 *   - run_shapes:    ephpm_db_run() over a SELECT, an INSERT and a zero-row
 *                    SELECT — proves has_rowset comes from the statement
 *   - empty_columns: ephpm_db_columns() after a zero-row SELECT
 *   - txn_state:     ephpm_db_in_transaction() around BEGIN/COMMIT/ROLLBACK
 *   - errno:         ephpm_db_errno()/ephpm_db_error() after a caught
 *                    exception, and their reset by the next statement
 *   - available:     ephpm_db_available()
 */

header('Content-Type: application/json');

$action = $_GET['action'] ?? 'query';

// Fail loudly rather than skip (#244): a missing native means the build
// under test does not have the surface these tests exist to cover.
$required = [
    'ephpm_db_query', 'ephpm_db_execute',
    // v0.7.4 adapter surface.
    'ephpm_db_run', 'ephpm_db_columns', 'ephpm_db_in_transaction',
    'ephpm_db_available', 'ephpm_db_errno', 'ephpm_db_error',
];
$missing = array_values(array_filter($required, static fn (string $f) => !function_exists($f)));
if ($missing !== []) {
    http_response_code(500);
    echo json_encode([
        'status' => 'error',
        'message' => 'native functions are not registered: ' . implode(', ', $missing),
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

        // ── v0.7.4 adapter surface ──────────────────────────────────────

        case 'run_shapes':
            // One entry point, three statement shapes. An adapter reads
            // has_rowset instead of classifying the SQL (#263).
            $sel = ephpm_db_run('SELECT id, name FROM bridge_e2e ORDER BY id');
            $ins = ephpm_db_run(
                'INSERT INTO bridge_e2e (name, value) VALUES (?, ?)',
                ['run_shapes', 'v']
            );
            $none = ephpm_db_run('SELECT id, name FROM bridge_e2e WHERE id = ?', [-1]);
            $noop = ephpm_db_run('SET NAMES utf8mb4');
            ephpm_db_execute('DELETE FROM bridge_e2e WHERE name = ?', ['run_shapes']);
            echo json_encode(['status' => 'ok', 'detail' => [
                'select_has_rowset'  => $sel['has_rowset'],
                'select_row_count'   => count($sel['rows']),
                'select_columns'     => array_column($sel['columns'], 'name'),
                'insert_has_rowset'  => $ins['has_rowset'],
                'insert_rows_is_arr' => is_array($ins['rows']),
                'insert_rows_count'  => count($ins['rows']),
                'insert_affected'    => $ins['affected_rows'],
                'insert_last_id_pos' => $ins['last_insert_id'] > 0,
                // The #262 case reached through the unified entry point:
                // no rows, but the columns are still described.
                'empty_has_rowset'   => $none['has_rowset'],
                'empty_row_count'    => count($none['rows']),
                'empty_columns'      => array_column($none['columns'], 'name'),
                'noop_has_rowset'    => $noop['has_rowset'],
                'noop_columns_count' => count($noop['columns']),
            ]]);
            break;

        case 'empty_columns':
            // A zero-row SELECT through the ORIGINAL entry point: the rows
            // are [] and cannot carry the names, so they come from the
            // companion native instead (#262).
            $rows = ephpm_db_query('SELECT id, name, value FROM bridge_e2e WHERE id = ?', [-1]);
            $cols = ephpm_db_columns();
            echo json_encode(['status' => 'ok', 'detail' => [
                'row_count'    => count($rows),
                'column_names' => array_column($cols, 'name'),
                // Declared types survive too; 'id' is the INTEGER PK.
                'id_type'      => strtoupper((string) ($cols[0]['type'] ?? '')),
            ]]);
            break;

        case 'txn_state':
            // #260: read the session's flag rather than tracking it in
            // userland or firing a blind ROLLBACK.
            $before = ephpm_db_in_transaction();
            ephpm_db_execute('BEGIN');
            $inside = ephpm_db_in_transaction();
            ephpm_db_execute(
                'INSERT INTO bridge_e2e (name, value) VALUES (?, ?)',
                ['txn_state', 'v']
            );
            $mid = ephpm_db_in_transaction();
            ephpm_db_execute('ROLLBACK');
            $after = ephpm_db_in_transaction();
            echo json_encode(['status' => 'ok', 'detail' => [
                'before' => $before,
                'inside' => $inside,
                'mid'    => $mid,
                'after'  => $after,
            ]]);
            break;

        case 'errno':
            // #259: the error is still readable after the exception has
            // been caught, and a later success clears it.
            $clean = ephpm_db_errno();
            $caught = null;
            try {
                ephpm_db_query('SELECT FROM WHERE (((');
            } catch (Exception $e) {
                $caught = $e->getCode();
            }
            $after_throw = ephpm_db_errno();
            $err = ephpm_db_error();
            ephpm_db_query('SELECT 1');
            echo json_encode(['status' => 'ok', 'detail' => [
                'errno_when_clean'    => $clean,
                'exception_code'      => $caught,
                'errno_after_throw'   => $after_throw,
                // The structured form agrees with the exception's code and
                // carries the SQLSTATE the message only embeds as text.
                'error_code'          => $err['code'] ?? null,
                'error_sqlstate'      => $err['sqlstate'] ?? null,
                'error_has_message'   => isset($err['message']) && $err['message'] !== '',
                'errno_after_success' => ephpm_db_errno(),
                'error_after_success' => ephpm_db_error(),
                // A SQL error never lands in the reserved client range.
                'code_is_reserved'    => $after_throw >= 2000 && $after_throw < 3000,
            ]]);
            break;

        case 'available':
            echo json_encode(['status' => 'ok', 'detail' => [
                'available' => ephpm_db_available(),
                // Distinct question from "am I on ePHPm".
                'declared'  => function_exists('ephpm_db_available'),
            ]]);
            break;

        default:
            http_response_code(400);
            echo json_encode(['status' => 'error', 'message' => "unknown action: {$action}"]);
    }
} catch (Exception $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'message' => $e->getMessage(), 'code' => $e->getCode()]);
}
