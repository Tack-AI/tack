//! Property tests for the CBOR wire framing: arbitrary bytes must never
//! panic the reader, and well-formed frames must round-trip.
#![allow(clippy::unwrap_used)]

use proptest::prelude::*;
use tack_protocol::schemas::{ClientMessage, PROTOCOL_VERSION};
use tack_protocol::{read_frame, write_frame};

proptest! {
    /// Garbage input: read_frame must return Ok(None)/Err, never panic.
    #[test]
    fn read_frame_never_panics_on_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut cursor = std::io::Cursor::new(bytes);
            let result = read_frame::<_, ClientMessage>(&mut cursor).await;
            let _ = result;
        });
    }

    /// Valid frames round-trip through write→read.
    #[test]
    fn frame_roundtrip(id in "[a-z0-9]{1,16}", text in ".*") {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let message = ClientMessage::Hello { version: PROTOCOL_VERSION, token: None, capabilities: Vec::new() };
            let mut buf: Vec<u8> = Vec::new();
            write_frame(&mut buf, &message).await.unwrap();
            let mut cursor = std::io::Cursor::new(buf);
            let back: Option<ClientMessage> = read_frame(&mut cursor).await.unwrap();
            prop_assert_eq!(back, Some(message));

            // A Request frame with arbitrary id/text.
            let request = ClientMessage::Request {
                id,
                request: tack_protocol::schemas::Command::Prompt {
                    session_id: "s".into(),
                    text,
                },
            };
            let mut buf: Vec<u8> = Vec::new();
            write_frame(&mut buf, &request).await.unwrap();
            let mut cursor = std::io::Cursor::new(buf);
            let back: Option<ClientMessage> = read_frame(&mut cursor).await.unwrap();
            prop_assert_eq!(back, Some(request));
            Ok(())
        })
        .unwrap();
    }
}
