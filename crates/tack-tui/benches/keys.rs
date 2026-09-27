//! Keystroke dispatch: `KeyEvent::matches` re-parses the spec string on
//! every call, and the app's global handler checks a keystroke against its
//! action list sequentially until one hits. Typed characters miss *every*
//! global action — that common path must stay cheap.

#![allow(clippy::unwrap_used)] // benches: panics are failures, keep code terse
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tack_tui::input::KeyEvent;
use tack_tui::keys::Keybindings;
use tack_tui::{Key, Modifiers};

/// Mirror of the global defaults in `tack-app/src/tui/mod.rs` (dispatch order
/// preserved); keep in sync when that list changes.
fn app_keybindings() -> (Keybindings, Vec<&'static str>) {
    let actions: Vec<(&str, &[&str])> = vec![
        ("app.interrupt", &["escape"]),
        ("app.clear", &["ctrl+c"]),
        ("app.exit", &["ctrl+d"]),
        ("app.tools.expand", &["ctrl+o"]),
        ("app.editor.external", &["ctrl+g"]),
        ("app.clipboard.pasteImage", &["ctrl+v", "alt+v"]),
        ("app.model.cycleForward", &["ctrl+p"]),
        ("app.model.cycleBackward", &["shift+ctrl+p", "ctrl+shift+p"]),
        ("app.model.select", &["ctrl+l"]),
        ("app.mode.cycle", &["shift+tab"]),
        ("app.thinking.toggle", &["ctrl+t"]),
        ("app.message.copy", &["ctrl+x"]),
        ("app.message.dequeue", &["alt+up"]),
        ("app.editor.historySearch", &["ctrl+r"]),
        ("app.message.sendNow", &["ctrl+enter", "ctrl+s"]),
        ("app.suspend", &["ctrl+z"]),
    ];
    let mut kb = Keybindings::new();
    for (action, specs) in &actions {
        kb.register(action, specs);
    }
    (kb, actions.iter().map(|(a, _)| *a).collect())
}

fn bench_event_matches(c: &mut Criterion) {
    let mut group = c.benchmark_group("key_event_matches");

    let enter = KeyEvent::plain(Key::Enter);
    group.bench_function("simple_hit", |b| {
        b.iter(|| black_box(&enter).matches(black_box("enter")))
    });

    let shift_ctrl_p = KeyEvent::new(
        Key::Char('p'),
        Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
        },
    );
    group.bench_function("modifiers_hit", |b| {
        b.iter(|| black_box(&shift_ctrl_p).matches(black_box("shift+ctrl+p")))
    });

    let typed = KeyEvent::plain(Key::Char('a'));
    group.bench_function("miss", |b| {
        b.iter(|| black_box(&typed).matches(black_box("ctrl+shift+p")))
    });

    group.finish();
}

fn bench_dispatch(c: &mut Criterion) {
    let (kb, actions) = app_keybindings();
    let mut group = c.benchmark_group("key_dispatch");

    // Typing path: a plain char is checked against every global action and
    // matches none — the hottest key path in the app.
    let typed = KeyEvent::plain(Key::Char('a'));
    group.bench_function("typed_char_misses_all", |b| {
        b.iter(|| {
            actions
                .iter()
                .any(|action| kb.matches(black_box(action), black_box(&typed)))
        })
    });

    // Worst-case hit: the matching action is checked last.
    let suspend = KeyEvent::ctrl(Key::Char('z'));
    group.bench_function("hit_last_action", |b| {
        b.iter(|| {
            actions
                .iter()
                .any(|action| kb.matches(black_box(action), black_box(&suspend)))
        })
    });

    group.finish();
}

criterion_group!(benches, bench_event_matches, bench_dispatch);
criterion_main!(benches);
