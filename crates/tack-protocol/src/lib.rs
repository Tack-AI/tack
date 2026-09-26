//! tack-protocol: framed-CBOR remote session protocol (wire-compatible with
//! `@earendil-works/pi-protocol` v1).

pub mod client;
pub mod framing;
pub mod schemas;

pub use client::RemoteClient;
pub use framing::{
    DEFAULT_MAX_FRAME_LENGTH, MAX_CBOR_NESTING_DEPTH, ProtocolError as FrameError, decode_payload,
    encode_frame, encode_payload, read_frame, write_frame,
};
pub use schemas::*;
