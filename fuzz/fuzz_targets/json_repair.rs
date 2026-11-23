#![no_main]

//! Tool-call arguments arrive from the model as partial / malformed JSON and
//! go through the lenient repair path. Weird input must not panic — the
//! worst case is a garbage `Value` the tool layer rejects.

use libfuzzer_sys::fuzz_target;
use tack_ai::json_repair::{parse_streaming_json, repair_json};
use tack_ai::streaming_json::ToolCallArgumentsStream;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let repaired = repair_json(&text);
    let _ = parse_streaming_json(&text);
    let _ = parse_streaming_json(&repaired);

    // Exercise the incremental state machine at an arbitrary UTF-8 boundary,
    // including the strict final path over the same untouched raw text.
    let mut cut = data.first().map_or(0, |byte| *byte as usize % (text.len() + 1));
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let (first, second) = text.split_at(cut);
    let mut stream = ToolCallArgumentsStream::new();
    let _ = stream.append(first);
    let _ = stream.append(second);
    let _ = stream.finish();
});
