#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes into the MySQL prepared statement ID parser.
// Invariant: must never panic. Short or malformed payloads must return None.
fuzz_target!(|data: &[u8]| {
    let result = ephpm_db::mysql::parse_stmt_id(data);

    // If the payload is at least 5 bytes, we should get Some(id).
    // If less than 5 bytes, we must get None.
    if data.len() < 5 {
        assert!(result.is_none(), "parse_stmt_id returned Some for short payload");
    }
});
