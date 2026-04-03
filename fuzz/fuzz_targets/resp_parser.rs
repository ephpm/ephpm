#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes into the RESP2 parser.
// Invariant: must never panic. Malformed input must return `Err` or `Ok(None)`.
fuzz_target!(|data: &[u8]| {
    let mut buf = BytesMut::from(data);

    // Try parsing frames until the buffer is exhausted or an error occurs.
    // This exercises the incremental parsing loop that the KV server uses.
    loop {
        match ephpm_kv::resp::parse_frame(&mut buf) {
            Ok(Some(_frame)) => {
                // Successfully parsed a frame — continue to see if there are more.
            }
            Ok(None) => {
                // Incomplete frame — parser needs more data. This is fine.
                break;
            }
            Err(_) => {
                // Protocol error — parser rejected the input gracefully.
                break;
            }
        }
    }
});
