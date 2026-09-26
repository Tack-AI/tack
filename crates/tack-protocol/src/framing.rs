//! Length-prefixed framing (4-byte big-endian) + CBOR codec (ciborium).
//! Matches `packages/protocol/src/framing.ts` and `codec.ts`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

/// Cap on CBOR container nesting during deserialization: serde walks one
/// stack frame per nesting level, so a frame of deeply nested arrays
/// (well under the 16 MiB byte cap) could otherwise overflow the pump
/// task's stack. Protocol messages are shallow; 128 levels is generous
/// headroom for any legitimate payload.
pub const MAX_CBOR_NESTING_DEPTH: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {0} exceeds limit {1}")]
    FrameTooLong(usize, usize),
    #[error("truncated frame at end of stream")]
    TruncatedFrame,
    /// The connection is closed (EOF, failed handshake read, or a pending
    /// request whose response can never arrive). Distinct from
    /// `TruncatedFrame`, which means the peer sent a partial frame.
    #[error("connection closed")]
    ConnectionClosed,
    /// A request exceeded the client-side timeout; its pending entry was
    /// cleaned up.
    #[error("request timed out")]
    Timeout,
    /// The server answered with a business-level error (distinct from
    /// transport/codec failures).
    #[error("server error {code:?}: {message}")]
    ServerError {
        code: crate::schemas::ProtocolErrorCode,
        message: String,
    },
    #[error("cbor encode error: {0}")]
    CborEncode(String),
    #[error("cbor decode error: {0}")]
    CborDecode(String),
}

/// Encode a value as a bare CBOR payload, without the 4-byte length prefix.
/// Used by transports that carry their own message boundaries (WebSocket:
/// one binary message = one CBOR payload).
pub fn encode_payload<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut payload = Vec::new();
    ciborium::into_writer(value, &mut payload)
        .map_err(|e| ProtocolError::CborEncode(e.to_string()))?;
    Ok(payload)
}

/// Encode a value as a framed CBOR payload.
pub fn encode_frame<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_payload(value)?;
    // TS throws a RangeError when the payload exceeds the u32 length prefix;
    // `as u32` would silently truncate.
    let length = u32::try_from(payload.len())
        .map_err(|_| ProtocolError::FrameTooLong(payload.len(), u32::MAX as usize))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode a CBOR payload. Nesting depth is capped at
/// [`MAX_CBOR_NESTING_DEPTH`] — the payload may come from an untrusted
/// peer and serde's recursive descent is unbounded otherwise.
pub fn decode_payload<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, ProtocolError> {
    ciborium::de::from_reader_with_recursion_limit(
        std::io::Cursor::new(payload),
        MAX_CBOR_NESTING_DEPTH,
    )
    .map_err(|e| ProtocolError::CborDecode(e.to_string()))
}

/// Write one framed message.
pub async fn write_frame<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), ProtocolError> {
    let frame = encode_frame(value)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one framed message (None on clean EOF before any bytes).
pub async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>, ProtocolError> {
    // Read the 4-byte header byte-by-byte: EOF before the first byte is a
    // clean end, but EOF mid-header is a truncated frame (TS
    // `FrameDecoder.end()` fails when headerLength !== 0). `read_exact`
    // conflates the two.
    let mut header = [0u8; 4];
    let mut header_len = 0usize;
    while header_len < header.len() {
        match reader.read(&mut header[header_len..]).await {
            Ok(0) => {
                return if header_len == 0 {
                    Ok(None)
                } else {
                    Err(ProtocolError::TruncatedFrame)
                };
            }
            Ok(n) => header_len += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > DEFAULT_MAX_FRAME_LENGTH {
        return Err(ProtocolError::FrameTooLong(
            length,
            DEFAULT_MAX_FRAME_LENGTH,
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::TruncatedFrame
        } else {
            ProtocolError::Io(e)
        }
    })?;
    Ok(Some(decode_payload(&payload)?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frame_roundtrip() {
        let value = json!({ "type": "hello", "version": 1, "nested": { "a": [1, 2, 3] } });
        let frame = encode_frame(&value).unwrap();
        assert_eq!(
            u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize,
            frame.len() - 4
        );

        let mut cursor = std::io::Cursor::new(frame);
        let decoded: serde_json::Value = read_frame::<_, serde_json::Value>(&mut cursor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decoded, value);
    }

    #[tokio::test]
    async fn truncated_frame_errors() {
        let mut cursor = std::io::Cursor::new(vec![0u8, 0, 0, 10, 1, 2]); // says 10, has 2
        let result = read_frame::<_, serde_json::Value>(&mut cursor).await;
        assert!(matches!(result, Err(ProtocolError::TruncatedFrame)));
    }

    #[tokio::test]
    async fn partial_header_is_truncated_not_clean_eof() {
        // 1-3 header bytes then EOF: a truncated frame, not a clean end
        // (TS FrameDecoder.end() fails with "Truncated frame at end of
        // stream" when headerLength !== 0).
        for n in 1..4usize {
            let mut cursor = std::io::Cursor::new(vec![0u8; n]);
            let result = read_frame::<_, serde_json::Value>(&mut cursor).await;
            assert!(
                matches!(result, Err(ProtocolError::TruncatedFrame)),
                "{n} header bytes then EOF must be TruncatedFrame, got {result:?}"
            );
        }
    }

    /// A frame of arrays nested past [`MAX_CBOR_NESTING_DEPTH`] is
    /// rejected before serde's recursive descent can exhaust the stack;
    /// shallow nesting decodes normally.
    #[test]
    fn deeply_nested_cbor_is_rejected() {
        // CBOR: N definite-length arrays of length 1 wrapped around null
        // (0x81 = array(1), 0xf6 = null) — one byte per nesting level.
        let mut deep = vec![0x81u8; MAX_CBOR_NESTING_DEPTH + 64];
        deep.push(0xf6);
        let result = decode_payload::<serde_json::Value>(&deep);
        assert!(
            matches!(result, Err(ProtocolError::CborDecode(_))),
            "{result:?}"
        );

        let mut shallow = vec![0x81u8; MAX_CBOR_NESTING_DEPTH / 2];
        shallow.push(0xf6);
        let decoded: serde_json::Value = decode_payload(&shallow).unwrap();
        let mut expected = json!(null);
        for _ in 0..MAX_CBOR_NESTING_DEPTH / 2 {
            expected = json!([expected]);
        }
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn clean_eof_is_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let result = read_frame::<_, serde_json::Value>(&mut cursor)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bare_payload_matches_framed_tail() {
        // WebSocket transport sends `encode_payload` (no prefix); TCP sends
        // `encode_frame`. Both must carry the identical CBOR bytes.
        let value = json!({ "type": "request", "id": "1", "request": { "command": "list" } });
        let frame = encode_frame(&value).unwrap();
        let payload = encode_payload(&value).unwrap();
        assert_eq!(&frame[4..], payload.as_slice());
        let decoded: serde_json::Value = decode_payload(&payload).unwrap();
        assert_eq!(decoded, value);
    }
}
