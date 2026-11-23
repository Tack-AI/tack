//! Streaming tool-call argument parsing. The legacy arm deliberately parses
//! every accumulated prefix, matching the old adapter hot path; the
//! incremental arm consumes the same chunks and snapshots at the same points.

#![allow(clippy::unwrap_used)] // benchmark setup must fail loudly on bad fixtures

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde_json::json;
use tack_ai::json_repair::parse_streaming_json;
use tack_ai::streaming_json::ToolCallArgumentsStream;

fn tool_arguments(target_bytes: usize) -> String {
    let unit = "let name = \"Tack\"; \\ path\nnext-line\tαβγ ";
    let content = unit.repeat((target_bytes / unit.len()).max(1));
    json!({
        "path": "src/main.rs",
        "edits": [{ "oldText": "before", "newText": content }],
        "mode": "write"
    })
    .to_string()
}

fn chunks(text: &str, size: usize) -> Vec<&str> {
    // The fixture's non-ASCII content is inside an escaped JSON string. Walk
    // char boundaries so every individual delta remains valid UTF-8.
    let mut result = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + size).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + text[start..].chars().next().unwrap().len_utf8();
        }
        result.push(&text[start..end]);
        start = end;
    }
    result
}

fn bench_streaming_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("streaming_tool_arguments");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(10);

    for target_bytes in [16 * 1024, 64 * 1024, 256 * 1024] {
        let raw = tool_arguments(target_bytes);
        let parts = chunks(&raw, 128);
        assert_eq!(parts.concat(), raw);
        let (finished, error) = ToolCallArgumentsStream::default().finish();
        assert_eq!(finished, json!({}));
        assert!(error.is_none());

        group.throughput(Throughput::Bytes(raw.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("legacy_reparse", raw.len()),
            &parts,
            |b, parts| {
                b.iter(|| {
                    let mut accumulated = String::new();
                    for delta in parts {
                        accumulated.push_str(delta);
                        black_box(parse_streaming_json(black_box(&accumulated)));
                    }
                    black_box(accumulated)
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("incremental", raw.len()),
            &parts,
            |b, parts| {
                b.iter(|| {
                    let mut stream = ToolCallArgumentsStream::new();
                    for delta in parts {
                        black_box(stream.append(black_box(delta)));
                    }
                    let (arguments, error) = stream.finish();
                    assert!(error.is_none());
                    black_box(arguments)
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_streaming_json);
criterion_main!(benches);
