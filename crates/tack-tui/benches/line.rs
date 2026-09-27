//! Line text hot paths: cell-width measurement (every render row calls it),
//! wrapping (markdown/streaming reflow), truncate + slice_cells (overflow and
//! horizontal scroll), and sanitize (all inbound text). Wide-character
//! (CJK/emoji) inputs exercise the grapheme-segmentation slow path.

#![allow(clippy::unwrap_used)] // benches: panics are failures, keep code terse
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use tack_tui::line::{Line, Span, sanitize};
use tack_tui::style::{Color, Style};

const COLS: usize = 120;

fn ascii_line() -> Line {
    let mut code = Style::new();
    code.fg = Some(Color::Indexed(109));
    Line::from_spans(vec![
        Span::styled("  42 ", code),
        Span::plain(
            "let rendered = renderer.render(&lines, width, height, true, &mut out)?; // a typical ASCII source row",
        ),
    ])
}

/// Mixed CJK + emoji + combining marks: forces grapheme segmentation and
/// per-cluster width lookup everywhere.
fn wide_line() -> Line {
    let mut bold = Style::new();
    bold.bold = true;
    Line::from_spans(vec![
        Span::styled("标题：", bold),
        Span::plain(
            " Rust 的终端渲染需要正确处理宽字符、组合符号（e\u{301}）和 emoji 👨‍👩‍👧‍👦，否则列对齐会错位。",
        ),
    ])
}

fn long_ascii() -> Line {
    Line::plain("lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ".repeat(20))
}

fn long_wide() -> Line {
    Line::plain("终端渲染需要按字形簇处理文本，宽字符占两列，组合符号占零列。".repeat(20))
}

fn bench_width(c: &mut Criterion) {
    let mut group = c.benchmark_group("line_width");

    let ascii = ascii_line();
    group.throughput(Throughput::Bytes(ascii.text().len() as u64));
    group.bench_function("ascii", |b| b.iter(|| black_box(&ascii).width()));

    let wide = wide_line();
    group.throughput(Throughput::Bytes(wide.text().len() as u64));
    group.bench_function("wide_chars", |b| b.iter(|| black_box(&wide).width()));

    group.finish();
}

fn bench_wrap(c: &mut Criterion) {
    let mut group = c.benchmark_group("line_wrap");

    let ascii = long_ascii();
    group.throughput(Throughput::Bytes(ascii.text().len() as u64));
    group.bench_function("long_ascii", |b| {
        b.iter(|| black_box(&ascii).wrap(black_box(COLS)))
    });

    let wide = long_wide();
    group.throughput(Throughput::Bytes(wide.text().len() as u64));
    group.bench_function("wide_chars", |b| {
        b.iter(|| black_box(&wide).wrap(black_box(COLS)))
    });

    // Already-fits fast path: must be ~free (clone of one line).
    let fits = ascii_line();
    group.bench_function("already_fits", |b| {
        b.iter(|| black_box(&fits).wrap(black_box(COLS)))
    });

    group.finish();
}

fn bench_overflow(c: &mut Criterion) {
    let mut group = c.benchmark_group("line_overflow");

    let wide = wide_line();
    group.bench_function("truncate_ellipsis", |b| {
        b.iter_batched(
            || wide.clone(),
            |mut l| {
                l.truncate(black_box(40), true);
                l
            },
            criterion::BatchSize::SmallInput,
        )
    });

    // Horizontal-scroll path: visible window out of a long line.
    let long = long_ascii();
    group.bench_function("slice_cells", |b| {
        b.iter(|| black_box(&long).slice_cells(black_box(200), black_box(COLS)))
    });

    group.finish();
}

fn bench_sanitize(c: &mut Criterion) {
    let mut group = c.benchmark_group("sanitize");

    // Clean text: borrowed fast path, must not allocate.
    let clean = "just an ordinary chat message with no control bytes at all, the common case";
    group.throughput(Throughput::Bytes(clean.len() as u64));
    group.bench_function("clean", |b| b.iter(|| sanitize(black_box(clean))));

    // Dirty text: control bytes interleaved (untrusted tool output).
    let dirty: String = (0..50).map(|i| format!("row{i}\x07\x1b[0m\x00")).collect();
    group.throughput(Throughput::Bytes(dirty.len() as u64));
    group.bench_function("dirty", |b| b.iter(|| sanitize(black_box(&dirty))));

    group.finish();
}

criterion_group!(
    benches,
    bench_width,
    bench_wrap,
    bench_overflow,
    bench_sanitize
);
criterion_main!(benches);
