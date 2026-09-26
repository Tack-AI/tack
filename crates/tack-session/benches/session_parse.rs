//! Session JSONL hot paths: `SessionLine::parse` (every prompt re-loads the
//! whole log) and `SessionLine::to_json` (every turn appends). Lines are raw
//! JSON in the v3 transactional shape; a setup assertion fails the bench if
//! schema drift pushes the samples onto the `Unknown` fallback (which would
//! silently benchmark the cheap path).

#![allow(clippy::unwrap_used)] // benches: panics are failures, keep code terse
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use tack_session::entry::SessionLine;

const LONG_OUTPUT: &str = include_str!("session_parse_long_output.txt");

fn realistic_lines(entries: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(entries + 1);
    lines.push(
        r#"{"type":"session","version":3,"id":"s-bench","timestamp":"2026-09-15T10:00:00.000Z","cwd":"/repo"}"#
            .to_string(),
    );
    for i in 0..entries {
        let line = match i % 4 {
            0 => format!(
                r#"{{"type":"message","id":"e{i}","parentId":"e{prev}","timestamp":"2026-09-15T10:01:{sec:02}.000Z","message":{{"role":"user","content":"refactor the renderer to avoid full-screen repaints; it is slow on large transcripts and flickers inside tmux","timestamp":1757930400000}}}}"#,
                prev = i.saturating_sub(1),
                sec = i % 60,
            ),
            1 => format!(
                r#"{{"type":"message","id":"e{i}","parentId":"e{prev}","timestamp":"2026-09-15T10:02:{sec:02}.000Z","message":{{"role":"bashExecution","command":"cargo test --workspace 2>&1 | tail -40","output":{output:?},"exitCode":0,"cancelled":false,"truncated":true,"fullOutputPath":"/tmp/full.log","timestamp":1757930401000}}}}"#,
                prev = i - 1,
                sec = i % 60,
                output = LONG_OUTPUT,
            ),
            2 => format!(
                r#"{{"type":"model_change","id":"e{i}","parentId":"e{prev}","timestamp":"2026-09-15T10:03:{sec:02}.000Z","provider":"anthropic","modelId":"claude-opus-4-8"}}"#,
                prev = i - 1,
                sec = i % 60,
            ),
            _ => format!(
                r#"{{"type":"thinking_level_change","id":"e{i}","parentId":"e{prev}","timestamp":"2026-09-15T10:04:{sec:02}.000Z","thinkingLevel":"high"}}"#,
                prev = i - 1,
                sec = i % 60,
            ),
        };
        lines.push(line);
    }
    lines
}

/// Every sample line must take the typed path; `Unknown`/`None` means the
/// bench no longer measures real parsing (schema drifted) — fail loudly.
fn assert_typed(lines: &[String]) {
    for (i, line) in lines.iter().enumerate() {
        match SessionLine::parse(line) {
            Some(SessionLine::Header(_)) | Some(SessionLine::Entry(_)) => {}
            other => panic!("line {i} did not take the typed path: {other:?}"),
        }
    }
}

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("session_jsonl");
    for entries in [1_000usize, 10_000, 50_000] {
        let lines = realistic_lines(entries);
        assert_typed(&lines);
        let bytes: u64 = lines.iter().map(|l| (l.len() + 1) as u64).sum();
        group.throughput(Throughput::Bytes(bytes));
        group.bench_with_input(BenchmarkId::new("parse", entries), &lines, |b, lines| {
            b.iter(|| {
                let mut parsed = 0usize;
                for line in lines {
                    if SessionLine::parse(black_box(line)).is_some() {
                        parsed += 1;
                    }
                }
                parsed
            });
        });
    }
    group.finish();
}

fn bench_serialize(c: &mut Criterion) {
    let lines = realistic_lines(10_000);
    let parsed: Vec<SessionLine> = lines.iter().filter_map(|l| SessionLine::parse(l)).collect();
    assert_eq!(parsed.len(), lines.len());
    c.bench_function("session_jsonl/serialize_10k", |b| {
        b.iter(|| {
            let mut bytes = 0usize;
            for line in &parsed {
                bytes += black_box(line).to_json().len();
            }
            bytes
        });
    });
}

criterion_group!(benches, bench_parse, bench_serialize);
criterion_main!(benches);
