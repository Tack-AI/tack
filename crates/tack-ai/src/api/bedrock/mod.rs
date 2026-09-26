//! Bedrock ConverseStream adapter. Port of `bedrock-converse-stream.ts` —
//! without the AWS SDK: requests are SigV4-signed by hand (`bedrock/sigv4.rs`)
//! and the response is the AWS event-stream binary framing
//! (`bedrock/eventstream.rs`).

pub mod convert;
pub mod credentials;
pub mod eventstream;
pub mod sigv4;
