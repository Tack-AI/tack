//! Shared test harness: an in-memory fake plugin driven over the other end
//! of a duplex pair, plus recording HostServices. Used by the tack-ext
//! integration tests (handshake.rs, tool_execute.rs).
// Each integration test binary compiles this module on its own; not every
// helper is used by every binary.
#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::Value;
use tack_ext::{Envelope, HostServices};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::Mutex;

/// HostServices that records every plugin→host request and event, answering
/// requests with `Ok(Value::Null)` (register is handled by the peer itself).
#[derive(Default, Debug)]
pub struct RecordingServices {
    pub requests: Mutex<Vec<(String, Value)>>,
    pub events: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl HostServices for RecordingServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.requests
            .lock()
            .await
            .push((method.to_string(), params));
        Ok(Value::Null)
    }
    async fn handle_event(&self, event: &str, payload: Value) {
        self.events.lock().await.push((event.to_string(), payload));
    }
}

/// The plugin-side ends of the transport: what the fake plugin reads
/// (host→plugin traffic) and writes (plugin→host traffic).
pub struct PluginSide {
    reader: BufReader<DuplexStream>,
    writer: DuplexStream,
}

impl PluginSide {
    /// Read one NDJSON line and parse it as an envelope.
    pub async fn read_envelope(&mut self) -> Envelope {
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.reader.read_line(&mut line),
        )
        .await
        .expect("plugin side timed out waiting for host line")
        .expect("read");
        assert!(!line.is_empty(), "host closed the connection");
        serde_json::from_str(line.trim_end()).expect("host sent invalid JSON")
    }

    /// Read one raw line (for byte-level wire assertions).
    pub async fn read_line(&mut self) -> String {
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.reader.read_line(&mut line),
        )
        .await
        .expect("plugin side timed out waiting for host line")
        .expect("read");
        assert!(!line.is_empty(), "host closed the connection");
        line.trim_end().to_string()
    }

    /// Send one envelope to the host.
    pub async fn send(&mut self, envelope: &Envelope) {
        let mut line = serde_json::to_string(envelope).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Send one raw line (malformed JSON, oversized payloads, ...).
    pub async fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Read and discard lines until a request with `method` arrives.
    pub async fn read_request(&mut self, method: &str) -> (u64, Value) {
        loop {
            match self.read_envelope().await {
                Envelope::Request {
                    id,
                    method: m,
                    params,
                } if m == method => {
                    return (id, params);
                }
                _ => continue,
            }
        }
    }
}

/// Wire a PluginPeer to a plugin side the test drives manually.
pub fn connect() -> (
    Arc<tack_ext::PluginPeer>,
    PluginSide,
    Arc<RecordingServices>,
) {
    let (host_reader, plugin_writer) = tokio::io::duplex(64 * 1024);
    let (plugin_reader, host_writer) = tokio::io::duplex(64 * 1024);
    let services = Arc::new(RecordingServices::default());
    let peer = tack_ext::PluginPeer::new(host_reader, host_writer, services.clone());
    let side = PluginSide {
        reader: BufReader::new(plugin_reader),
        writer: plugin_writer,
    };
    (peer, side, services)
}

/// A ready-to-use register payload for the fake plugin.
pub fn register_payload(name: &str) -> Value {
    serde_json::json!({
        "name": name,
        "tools": [{
            "name": "ping",
            "label": "Ping",
            "description": "answers pong",
            "parameters": {"type": "object", "properties": {}},
        }],
        "commands": [{"name": "hello", "description": "greets"}],
        "shortcuts": [{"action": "ext.fake.ping", "keys": ["ctrl+p"]}],
        "subscriptions": ["tool_call"],
    })
}

/// The initialize payload the host should send, built like the app builds it.
pub fn initialize_payload() -> tack_ext::InitializePayload {
    tack_ext::InitializePayload {
        protocol: tack_ext::PROTOCOL_VERSION,
        mode: "tui".to_string(),
        cwd: "/tmp/tack-ext-test".to_string(),
        trusted: true,
        host: "tack-test/0.0".to_string(),
    }
}

/// Fake plugin: expect initialize, check the protocol version against
/// `supported`, then register. Returns the initialize payload the host sent
/// (for assertions); on version mismatch the plugin exits without
/// registering (plugins have no error channel during the handshake — the
/// only defined rejection is to refuse the session).
pub async fn run_versioned_handshake(mut side: PluginSide, supported: u32) -> Value {
    let init = side.read_envelope().await;
    let Envelope::Event { event, payload } = init else {
        panic!("expected initialize event")
    };
    assert_eq!(event, "initialize");
    if payload["protocol"].as_u64().unwrap() != u64::from(supported) {
        // Exit: drop both ends without registering.
        return payload;
    }
    side.send(&Envelope::event("register", register_payload("fake")))
        .await;
    payload
}
