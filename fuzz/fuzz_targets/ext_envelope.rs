#![no_main]

//! Plugins are separate processes feeding NDJSON envelopes to the host over
//! stdio. A malformed envelope must be a protocol error, never a host panic
//! (plugins are lower-trust than the host).

use libfuzzer_sys::fuzz_target;
use tack_ext::protocol::Envelope;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    // The host reads line-delimited frames; probe both the whole blob and
    // each line on its own.
    if let Ok(envelope) = serde_json::from_str::<Envelope>(&text) {
        let round = serde_json::to_string(&envelope).expect("envelope serializes");
        let _ = serde_json::from_str::<Envelope>(&round);
    }
    for line in text.lines() {
        let _ = serde_json::from_str::<Envelope>(line);
    }
});
