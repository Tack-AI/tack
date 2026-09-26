#![no_main]

//! The remote-session protocol decodes CBOR from a network peer: malformed
//! frames must come back as `Err`, never panic the server.

use libfuzzer_sys::fuzz_target;
use tack_protocol::framing::decode_payload;
use tack_protocol::schemas::{SessionSnapshot, TranscriptItem};

fuzz_target!(|data: &[u8]| {
    let _ = decode_payload::<SessionSnapshot>(data);
    let _ = decode_payload::<TranscriptItem>(data);
    let _ = decode_payload::<serde_json::Value>(data);
});
