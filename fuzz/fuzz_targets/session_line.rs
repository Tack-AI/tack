#![no_main]

//! Session files are user data read back on every prompt: one malformed
//! line must degrade to `Unknown`/skip, never panic the agent.

use libfuzzer_sys::fuzz_target;
use tack_session::entry::SessionLine;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let Some(line) = SessionLine::parse(&text) else {
        return;
    };
    // Whatever parses must re-serialize and re-parse without panicking
    // (the v3 append path does exactly this round-trip).
    let json = line.to_json();
    let _ = SessionLine::parse(&json);
});
