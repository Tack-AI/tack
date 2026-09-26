#![no_main]

//! Tool-call arguments arrive from the model as partial / malformed JSON and
//! go through the lenient repair path. Weird input must not panic — the
//! worst case is a garbage `Value` the tool layer rejects.

use libfuzzer_sys::fuzz_target;
use tack_ai::json_repair::{parse_streaming_json, repair_json};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let repaired = repair_json(&text);
    let _ = parse_streaming_json(&text);
    let _ = parse_streaming_json(&repaired);
});
