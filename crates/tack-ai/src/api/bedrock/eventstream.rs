//! AWS event-stream binary frame decoder (the ConverseStream response
//! wire format): 12-byte prelude (total length, headers length, prelude
//! CRC32), typed headers, payload, trailing message CRC32. Incremental:
//! frames may split or coalesce across TCP chunks.

#[derive(Clone, Debug, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Int(i32),
    Long(i64),
    String(String),
    Bytes(Vec<u8>),
    /// Timestamp millis / uuid bytes / etc. kept raw.
    Raw(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn header_str(&self, name: &str) -> Option<&str> {
        match self.header(name) {
            Some(HeaderValue::String(s)) => Some(s),
            _ => None,
        }
    }

    /// `:event-type` header value.
    pub fn event_type(&self) -> Option<&str> {
        self.header_str(":event-type")
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FrameError {
    #[error("frame too short")]
    TooShort,
    #[error("prelude CRC mismatch")]
    PreludeCrc,
    #[error("message CRC mismatch")]
    MessageCrc,
    #[error("malformed header")]
    MalformedHeader,
    #[error("invalid frame length")]
    InvalidLength,
    #[error("frame too large")]
    TooLarge,
}

/// Hard cap on one frame's declared total length. Without it a malformed
/// stream can claim a ~4 GiB frame and the decoder would buffer arbitrary
/// amounts waiting for bytes that never come (memory DoS). ConverseStream
/// payloads are single events — 16 MiB is generous headroom.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct EventStreamDecoder {
    buf: Vec<u8>,
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns all complete frames now available.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, FrameError> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            match self.try_take_frame() {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }

    fn try_take_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buf.len() < 12 {
            return Ok(None);
        }
        let total_len =
            u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        let headers_len =
            u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
        if total_len < 16 || headers_len > total_len - 16 {
            return Err(FrameError::InvalidLength);
        }
        if total_len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge);
        }
        if self.buf.len() < total_len {
            return Ok(None);
        }
        let prelude_crc =
            u32::from_be_bytes([self.buf[8], self.buf[9], self.buf[10], self.buf[11]]);
        if crc32fast::hash(&self.buf[..8]) != prelude_crc {
            return Err(FrameError::PreludeCrc);
        }
        if crc32fast::hash(&self.buf[..total_len - 4])
            != u32::from_be_bytes([
                self.buf[total_len - 4],
                self.buf[total_len - 3],
                self.buf[total_len - 2],
                self.buf[total_len - 1],
            ])
        {
            return Err(FrameError::MessageCrc);
        }
        let headers = parse_headers(&self.buf[12..12 + headers_len])?;
        let payload = self.buf[12 + headers_len..total_len - 4].to_vec();
        self.buf.drain(..total_len);
        Ok(Some(Frame { headers, payload }))
    }
}

fn take<'a>(value: &mut &'a [u8], n: usize) -> Result<&'a [u8], FrameError> {
    if value.len() < n {
        return Err(FrameError::MalformedHeader);
    }
    let (head, tail) = value.split_at(n);
    *value = tail;
    Ok(head)
}

fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, HeaderValue)>, FrameError> {
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let (name_len, rest) = bytes.split_first().ok_or(FrameError::MalformedHeader)?;
        let name_len = *name_len as usize;
        if rest.len() < name_len + 1 {
            return Err(FrameError::MalformedHeader);
        }
        let name = String::from_utf8_lossy(&rest[..name_len]).to_string();
        let value_type = rest[name_len];
        let mut value = &rest[name_len + 1..];
        let parsed = match value_type {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Int(take(&mut value, 1)?[0] as i32),
            3 => {
                let b = take(&mut value, 2)?;
                HeaderValue::Int(i16::from_be_bytes([b[0], b[1]]) as i32)
            }
            4 => {
                let b = take(&mut value, 4)?;
                HeaderValue::Int(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            }
            5 => {
                let b = take(&mut value, 8)?;
                HeaderValue::Long(i64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            6 => {
                let b = take(&mut value, 2)?;
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                HeaderValue::Bytes(take(&mut value, len)?.to_vec())
            }
            7 => {
                let b = take(&mut value, 2)?;
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                HeaderValue::String(String::from_utf8_lossy(take(&mut value, len)?).to_string())
            }
            8 => HeaderValue::Raw(take(&mut value, 8)?.to_vec()),
            9 => HeaderValue::Raw(take(&mut value, 16)?.to_vec()),
            _ => return Err(FrameError::MalformedHeader),
        };
        headers.push((name, parsed));
        bytes = value;
    }
    Ok(headers)
}

/// Build a frame (used by adapter tests to script ConverseStream responses).
pub fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7u8); // string
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total_len = (12 + header_bytes.len() + payload.len() + 4) as u32;
    let mut frame = Vec::new();
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    let prelude_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    let message_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn sample_frame() -> Vec<u8> {
        build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "messageStart"),
                (":content-type", "application/json"),
            ],
            br#"{"role":"assistant"}"#,
        )
    }

    #[test]
    fn decodes_whole_frame() {
        let mut decoder = EventStreamDecoder::new();
        let frames = decoder.feed(&sample_frame()).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type(), Some("messageStart"));
        assert_eq!(frames[0].payload, br#"{"role":"assistant"}"#);
    }

    #[test]
    fn splits_at_every_offset() {
        let frame = sample_frame();
        for split in 0..frame.len() {
            let mut decoder = EventStreamDecoder::new();
            let mut frames = decoder.feed(&frame[..split]).unwrap();
            frames.extend(decoder.feed(&frame[split..]).unwrap());
            assert_eq!(frames.len(), 1, "split at {split}");
            assert_eq!(frames[0].event_type(), Some("messageStart"));
        }
    }

    #[test]
    fn coalesced_frames() {
        let mut both = sample_frame();
        both.extend_from_slice(&sample_frame());
        let mut decoder = EventStreamDecoder::new();
        assert_eq!(decoder.feed(&both).unwrap().len(), 2);
    }

    #[test]
    fn oversized_frame_length_errors() {
        // A frame declaring a 17 MiB total length must be rejected up front
        // instead of buffering until the bytes arrive.
        let mut frame = sample_frame();
        let oversized = (17 * 1024 * 1024u32).to_be_bytes();
        frame[..4].copy_from_slice(&oversized);
        let mut decoder = EventStreamDecoder::new();
        assert_eq!(decoder.feed(&frame), Err(FrameError::TooLarge));
    }

    #[test]
    fn corrupt_crc_errors() {
        let mut frame = sample_frame();
        let n = frame.len();
        frame[n - 1] ^= 0xff; // message CRC
        let mut decoder = EventStreamDecoder::new();
        assert_eq!(decoder.feed(&frame), Err(FrameError::MessageCrc));

        let mut frame = sample_frame();
        frame[8] ^= 0xff; // prelude CRC
        let mut decoder = EventStreamDecoder::new();
        assert_eq!(decoder.feed(&frame), Err(FrameError::PreludeCrc));
    }
}
