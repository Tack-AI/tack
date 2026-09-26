//! Diff-renderer hot paths: full-screen first paint, steady-state re-render
//! of an unchanged frame (the keystroke path — must stay ~free), and the
//! append-one-line streaming case.

#![allow(clippy::unwrap_used)] // benches: panics are failures, keep code terse
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use tack_tui::line::{Line, Span};
use tack_tui::screen_main::MainScreenRenderer;
use tack_tui::style::{Color, Style};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 50;

fn frame(lines: usize) -> Vec<Line> {
    let mut code_style = Style::new();
    code_style.fg = Some(Color::Indexed(109));
    let mut bold = Style::new();
    bold.bold = true;
    (0..lines)
        .map(|i| {
            if i % 10 == 0 {
                Line::from_spans(vec![
                    Span::styled(format!("## section {i} "), bold),
                    Span::plain("transcript header line"),
                ])
            } else {
                Line::from_spans(vec![
                    Span::styled(format!("{i:>5} "), code_style),
                    Span::plain("let rendered = renderer.render(&lines, width, height, true, &mut out)?; // typical row"),
                ])
            }
        })
        .collect()
}

fn bench_render(c: &mut Criterion) {
    let lines = frame(2_000);
    let approx_bytes: u64 = lines.iter().map(|l| l.width() as u64 + 8).sum();

    let mut group = c.benchmark_group("tui_render");
    group.throughput(Throughput::Bytes(approx_bytes));

    // First paint: every row is emitted.
    group.bench_function("cold_full_frame", |b| {
        b.iter_batched(
            MainScreenRenderer::new,
            |mut r| {
                let mut out = Vec::new();
                r.render(black_box(&lines), WIDTH, HEIGHT, true, &mut out)
                    .unwrap();
                out
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Steady state: re-rendering an unchanged frame must emit almost nothing.
    group.bench_function("steady_unchanged", |b| {
        let mut r = MainScreenRenderer::new();
        let mut out = Vec::new();
        r.render(&lines, WIDTH, HEIGHT, true, &mut out).unwrap();
        b.iter(|| {
            out.clear();
            r.render(black_box(&lines), WIDTH, HEIGHT, true, &mut out)
                .unwrap();
        });
    });

    // Streaming: one line appended to an already-rendered frame.
    group.bench_function("append_one_line", |b| {
        b.iter_batched(
            || {
                let mut r = MainScreenRenderer::new();
                let mut out = Vec::new();
                r.render(&lines, WIDTH, HEIGHT, true, &mut out).unwrap();
                let mut extended = lines.clone();
                extended.push(Line::plain("new streamed row"));
                (r, extended)
            },
            |(mut r, extended)| {
                let mut out = Vec::new();
                r.render(black_box(&extended), WIDTH, HEIGHT, true, &mut out)
                    .unwrap();
                out
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
