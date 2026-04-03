#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary strings into the MySQL query classifier.
// Invariant: must never panic. Any input string must produce a valid QueryKind.
fuzz_target!(|data: &[u8]| {
    let sql = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return,
    };

    // classify_mysql_query must always return a valid enum variant, never panic.
    let _kind = ephpm_db::mysql::classify_mysql_query(sql);
});
