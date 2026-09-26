//! Syntax highlighting (syntect): batch-highlighting a code block vs
//! per-line highlighting, the memo/incremental caches behind
//! `highlight_block`, plus the unknown-language fallback. The SyntaxSet and
//! theme load lazily behind a OnceLock on first use — benches warm them up
//! in setup, so numbers reflect steady state, not the one-time load.
//!
//! Real-work benches use `highlight_block_uncached`: `highlight_block`
//! memoizes exact repeats, which would otherwise measure a cache hit.

#![allow(clippy::unwrap_used)] // benches: panics are failures, keep code terse
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use tack_tui::syntax::{highlight_block, highlight_block_uncached, highlight_line};

/// A real Rust source file as the large input (~900 lines).
const RUST_SOURCE: &str = include_str!("../src/screen_main.rs");

/// Typical fenced code block in a chat transcript.
fn small_block() -> String {
    r#"fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--help") => println!("usage: demo [file]"),
        Some(path) => println!("opening {path}"),
        None => eprintln!("missing argument"),
    }
}
"#
    .to_string()
}

fn bench_highlight(c: &mut Criterion) {
    // Warm the lazily-loaded SyntaxSet + theme before measuring.
    highlight_line("fn warmup() {}", "rust");

    let mut group = c.benchmark_group("syntax");

    // Large file, one highlighter across all lines (the correct batch API).
    group.throughput(Throughput::Bytes(RUST_SOURCE.len() as u64));
    group.bench_function("block_rust_large", |b| {
        b.iter(|| highlight_block_uncached(black_box(RUST_SOURCE), black_box("rust")))
    });

    // Same file highlighted line-by-line: recreates the highlighter per
    // call — quantifies why callers must use a block-level API instead.
    group.bench_function("line_by_line_rust_large", |b| {
        b.iter(|| {
            for line in RUST_SOURCE.lines() {
                black_box(highlight_line(black_box(line), black_box("rust")));
            }
        })
    });

    // Small block: the dominant chat case (a fenced snippet per message).
    let small = small_block();
    group.throughput(Throughput::Bytes(small.len() as u64));
    group.bench_function("block_rust_small", |b| {
        b.iter(|| highlight_block_uncached(black_box(&small), black_box("rust")))
    });

    // Memo hit: identical content re-rendered (terminal resize, scroll-back
    // of an evicted transcript entry) — must be ~free.
    group.bench_function("memo_hit_rust_large", |b| {
        highlight_block(RUST_SOURCE, "rust"); // seed the memo once
        b.iter(|| highlight_block(black_box(RUST_SOURCE), black_box("rust")))
    });

    // Streaming append: one more line on an already-highlighted growing
    // block — the per-throttle-pass cost with the incremental cache (was
    // O(block) per pass before). Setup re-seeds the entry (unmeasured).
    group.bench_function("stream_append_line_incremental", |b| {
        b.iter_batched(
            || highlight_block(RUST_SOURCE, "rust"),
            |_| {
                let mut extended = String::from(RUST_SOURCE);
                extended.push_str("\n// one more streamed line");
                highlight_block(black_box(&extended), black_box("rust"))
            },
            criterion::BatchSize::SmallInput,
        )
    });

    // Unknown language: plain-text fallback, must be ~free.
    group.bench_function("unknown_lang_fallback", |b| {
        b.iter(|| highlight_block(black_box(RUST_SOURCE), black_box("notalang")))
    });

    group.finish();
}

criterion_group!(benches, bench_highlight);
criterion_main!(benches);
