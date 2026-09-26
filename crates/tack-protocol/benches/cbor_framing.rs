//! Remote-session wire format: CBOR payload encode/decode of a
//! `SessionSnapshot` (the fat message `tack serve` ships on every state
//! change) and the framed variant used on the socket.

#![allow(clippy::unwrap_used)] // benches: panics are failures, keep code terse
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use tack_protocol::framing::{decode_payload, encode_frame, encode_payload};
use tack_protocol::schemas::{
    AssistantContent, ModelRef, SessionPhase, SessionSnapshot, ThinkingLevel, TranscriptItem,
    Usage, UsageCost, UserContent,
};

fn sample_snapshot(items: usize) -> SessionSnapshot {
    let text = "The diff renderer now only rewrites changed rows; steady-state keystrokes are O(changed) instead of O(transcript).".repeat(2);
    let mut transcript = Vec::with_capacity(items);
    for i in 0..items {
        let item = if i % 2 == 0 {
            TranscriptItem::User {
                id: format!("t{i}"),
                content: vec![UserContent::Text { text: text.clone() }],
                timestamp: 1_757_930_400_000 + i as u64,
            }
        } else {
            TranscriptItem::Assistant {
                id: format!("t{i}"),
                content: vec![AssistantContent::Text { text: text.clone() }],
                model: ModelRef {
                    provider: "anthropic".into(),
                    id: "claude-opus-4-8".into(),
                },
                response_model: None,
                usage: Some(Usage {
                    input: 12_345,
                    output: 678,
                    cache_read: 9_000,
                    cache_write: 1_000,
                    reasoning: Some(120),
                    total_tokens: 22_145,
                    cost: UsageCost {
                        input: 0.012,
                        output: 0.034,
                        cache_read: 0.001,
                        cache_write: 0.002,
                        total: 0.049,
                    },
                }),
                timestamp: 1_757_930_400_000 + i as u64,
                status: "complete".into(),
                stop_reason: Some("endTurn".into()),
                error_message: None,
            }
        };
        transcript.push(item);
    }
    SessionSnapshot {
        id: "s-bench".into(),
        name: Some("bench session".into()),
        cwd: "/repo".into(),
        created_at: 1_757_930_400_000,
        updated_at: 1_757_934_000_000,
        phase: SessionPhase::Turn,
        model: ModelRef {
            provider: "anthropic".into(),
            id: "claude-opus-4-8".into(),
        },
        thinking_level: ThinkingLevel::Medium,
        attached: true,
        locked: false,
        revision: 42,
        mode: None,
        transcript,
        queued_steer: Vec::new(),
        queued_steer_count: 0,
    }
}

fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("cbor_snapshot");
    for items in [50usize, 500, 2_000] {
        let snapshot = sample_snapshot(items);
        let encoded = encode_payload(&snapshot).expect("encodes");
        let size = encoded.len() as u64;

        group.throughput(Throughput::Bytes(size));
        group.bench_with_input(
            BenchmarkId::new("encode", items),
            &snapshot,
            |b, snapshot| {
                b.iter(|| encode_payload(black_box(snapshot)).expect("encodes"));
            },
        );
        group.bench_with_input(BenchmarkId::new("decode", items), &encoded, |b, encoded| {
            b.iter(|| decode_payload::<SessionSnapshot>(black_box(encoded)).expect("decodes"));
        });
        group.bench_with_input(
            BenchmarkId::new("encode_frame", items),
            &snapshot,
            |b, snapshot| b.iter(|| encode_frame(black_box(snapshot)).expect("encodes")),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_roundtrip);
criterion_main!(benches);
