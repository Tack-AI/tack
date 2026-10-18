//! Reproduce main-screen diff renderer behavior with a virtual screen:
//! drive frames like the TUI does (transcript growth + spinner ticks) and
//! interpret the emitted ANSI to catch duplicate blocks / cursor bugs.
#![allow(clippy::unwrap_used)]

use tack_tui::screen_main::MainScreenRenderer;
use tack_tui::{Line, Span};

/// Minimal virtual screen: text cells only (styles ignored).
#[derive(Debug)]
struct VirtualScreen {
    rows: Vec<String>,
    cursor_row: usize,
}

impl VirtualScreen {
    fn new(_height: usize) -> Self {
        VirtualScreen {
            rows: Vec::new(),
            cursor_row: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).to_string();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => {
                    // CSI … letter or OSC ]…BEL
                    match chars.next() {
                        Some('[') => {
                            let mut params = String::new();
                            let mut command = String::new();
                            for ch in chars.by_ref() {
                                if ch.is_ascii_alphabetic() {
                                    command.push(ch);
                                    break;
                                }
                                params.push(ch);
                            }
                            self.csi(&params, &command);
                        }
                        Some(']') => {
                            // OSC: consume to BEL or ST
                            let mut prev = '\0';
                            for ch in chars.by_ref() {
                                if ch == '\x07' || (prev == '\x1b' && ch == '\\') {
                                    break;
                                }
                                prev = ch;
                            }
                        }
                        _ => {}
                    }
                }
                '\r' => {}
                '\n' => {
                    self.cursor_row += 1;
                    if self.cursor_row >= self.rows.len() {
                        self.rows.push(String::new());
                    }
                }
                c => {
                    let row = self.cursor_row.min(self.rows.len().saturating_sub(1));
                    if self.rows.len() <= row {
                        self.rows.push(String::new());
                    }
                    self.rows[row].push(c);
                }
            }
        }
    }

    fn csi(&mut self, params: &str, command: &str) {
        let n: usize = params.parse().unwrap_or(1);
        match command {
            "A" => self.cursor_row = self.cursor_row.saturating_sub(n),
            "B" => self.cursor_row += n,
            "H" => self.cursor_row = 0,
            "J" => {
                if params == "2" {
                    self.rows.clear();
                    self.cursor_row = 0;
                }
            }
            "K" if params == "2" => {
                let row = self.cursor_row.min(self.rows.len().saturating_sub(1));
                self.rows[row].clear();
            }
            "L" => {
                let row = self.cursor_row.min(self.rows.len());
                for _ in 0..n {
                    self.rows.insert(row, String::new());
                }
            }
            "M" => {
                let row = self.cursor_row.min(self.rows.len().saturating_sub(1));
                for _ in 0..n {
                    if row < self.rows.len() {
                        self.rows.remove(row);
                    }
                }
            }
            _ => {}
        }
    }

    fn dump(&self) -> String {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, r)| format!("{i:3}|{r}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn frame_lines(items: &[&str], spinner: &str, footer: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = items.iter().map(|s| Line::plain(*s)).collect();
    lines.push(Line::plain(format!("⠋ {spinner}")));
    lines.push(Line::plain("> "));
    lines.push(Line::from_spans(vec![Span::plain(footer), Span::plain("")]));
    lines
}

#[test]
fn debug_diff_renderer() {
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut screen = VirtualScreen::new(50);

    // Frame 1: one item + status + editor + footer.
    renderer
        .render(
            &frame_lines(&["1. item one"], "Working…", "footer-1 | footer-2"),
            80,
            50,
            false,
            &mut out,
        )
        .unwrap();
    screen.feed(&out);
    let after1 = screen.dump();
    println!("=== after frame 1 ===\n{after1}");

    // Spinner tick: same content, only spinner char changes.
    out.clear();
    renderer
        .render(
            &frame_lines(&["1. item one"], "Working.", "footer-1 | footer-2"),
            80,
            50,
            false,
            &mut out,
        )
        .unwrap();
    screen.feed(&out);
    println!("=== after tick ===\n{}", screen.dump());

    // New transcript line streams in.
    out.clear();
    renderer
        .render(
            &frame_lines(
                &["1. item one", "2. item two"],
                "Working…",
                "footer-1 | footer-2",
            ),
            80,
            50,
            false,
            &mut out,
        )
        .unwrap();
    screen.feed(&out);
    println!("=== after frame 2 ===\n{}", screen.dump());

    // Third line arrives.
    out.clear();
    renderer
        .render(
            &frame_lines(
                &["1. item one", "2. item two", "3. item three"],
                "Working…",
                "footer-1 | footer-2",
            ),
            80,
            50,
            false,
            &mut out,
        )
        .unwrap();
    screen.feed(&out);
    let final_dump = screen.dump();
    println!("=== after frame 3 ===\n{final_dump}");

    // No duplicated status/footer blocks.
    assert_eq!(
        final_dump.matches("Working").count(),
        1,
        "status duplicated:\n{final_dump}"
    );
    assert_eq!(
        final_dump.matches("footer-1").count(),
        1,
        "footer duplicated:\n{final_dump}"
    );
    assert_eq!(
        final_dump.matches("> ").count(),
        1,
        "editor duplicated:\n{final_dump}"
    );
}

/// Same scenario but the content exceeds the viewport height (real TUI
/// conditions: long transcript, small screen) and the editor emits the
/// hardware cursor marker.
#[test]
fn debug_diff_renderer_viewport_overflow() {
    let height = 5u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut screen = VirtualScreen::new(height as usize);

    let marker = "\x1b_pi:c\x07";
    let frame = |items: &[&str], spinner: &str| -> Vec<Line> {
        let mut lines: Vec<Line> = items.iter().map(|s| Line::plain(*s)).collect();
        lines.push(Line::plain(format!("⠋ {spinner}")));
        lines.push(Line::plain(format!("> {marker}")));
        lines.push(Line::plain("footer"));
        lines
    };

    let mut items: Vec<&str> = vec!["1. one"];
    for step in 0..8 {
        out.clear();
        renderer
            .render(&frame(&items, "Working…"), 80, height, false, &mut out)
            .unwrap();
        screen.feed(&out);
        // Spinner tick.
        out.clear();
        renderer
            .render(&frame(&items, "Working."), 80, height, false, &mut out)
            .unwrap();
        screen.feed(&out);
        items.push(match step {
            0 => "2. two",
            1 => "3. three",
            2 => "4. four",
            3 => "5. five",
            4 => "6. six",
            5 => "7. seven",
            6 => "8. eight",
            _ => "9. nine",
        });
    }
    let dump = screen.dump();
    println!("=== overflow final ===\n{dump}");
    let status_count = dump.matches("Working").count();
    let footer_count = dump.matches("footer").count();
    assert_eq!(status_count, 1, "status duplicated:\n{dump}");
    assert_eq!(footer_count, 1, "footer duplicated:\n{dump}");
}

/// Faithful fixed-size virtual terminal: width×height grid, deferred autowrap,
/// newline scrolls at the bottom row, CUD/CUF clamp at the edges.
/// The naive VirtualScreen above grows unboundedly and cannot catch
/// viewport/scroll bugs — this one can.
#[derive(Debug)]
struct VirtualTerm {
    rows: Vec<Vec<char>>,
    width: usize,
    height: usize,
    cr: usize, // cursor row
    cc: usize, // cursor col
    pending_wrap: bool,
    /// Total lines scrolled off the top (for diagnostics).
    scrolled: usize,
    /// Rows that scrolled off the top, in order (terminal scrollback).
    scrollback: Vec<String>,
}

impl VirtualTerm {
    fn new(width: usize, height: usize) -> Self {
        VirtualTerm {
            rows: vec![Vec::new(); height],
            width,
            height,
            cr: 0,
            cc: 0,
            pending_wrap: false,
            scrolled: 0,
            scrollback: Vec::new(),
        }
    }

    fn newline(&mut self) {
        self.pending_wrap = false;
        if self.cr == self.height - 1 {
            let removed = self.rows.remove(0);
            self.scrollback.push(removed.iter().collect());
            self.rows.push(Vec::new());
            self.scrolled += 1;
        } else {
            self.cr += 1;
        }
        self.cc = 0;
    }

    fn put_char(&mut self, c: char) {
        if self.pending_wrap {
            self.newline();
        }
        self.rows[self.cr].truncate(self.cc);
        while self.rows[self.cr].len() < self.cc {
            self.rows[self.cr].push(' ');
        }
        if self.rows[self.cr].len() == self.cc {
            self.rows[self.cr].push(c);
        } else {
            self.rows[self.cr][self.cc] = c;
        }
        if self.cc + 1 >= self.width {
            self.pending_wrap = true;
        } else {
            self.cc += 1;
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).to_string();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut command = String::new();
                        for ch in chars.by_ref() {
                            if ch.is_ascii_alphabetic() {
                                command.push(ch);
                                break;
                            }
                            params.push(ch);
                        }
                        self.csi(&params, &command);
                    }
                    Some(']') => {
                        let mut prev = '\0';
                        for ch in chars.by_ref() {
                            if ch == '\x07' || (prev == '\x1b' && ch == '\\') {
                                break;
                            }
                            prev = ch;
                        }
                    }
                    _ => {}
                },
                '\r' => {
                    self.cc = 0;
                    self.pending_wrap = false;
                }
                '\n' => self.newline(),
                c if (c as u32) >= 0x20 => self.put_char(c),
                _ => {}
            }
        }
    }

    fn csi(&mut self, params: &str, command: &str) {
        // Private-mode sequences (e.g. ?2026h/l) end in a letter too; ignore.
        let clean = params.trim_start_matches('?');
        let n: usize = clean.parse().unwrap_or(1);
        match command {
            "A" => self.cr = self.cr.saturating_sub(n),
            "B" => self.cr = (self.cr + n).min(self.height - 1),
            "C" => self.cc = (self.cc + n).min(self.width - 1),
            "D" => self.cc = self.cc.saturating_sub(n),
            "H" => {
                self.cr = 0;
                self.cc = 0;
            }
            "J" => {
                if clean == "2" {
                    for row in &mut self.rows {
                        row.clear();
                    }
                    self.cr = 0;
                    self.cc = 0;
                } else if clean == "3" {
                    // Clear scrollback, leaving the (already cleared) screen.
                    self.scrollback.clear();
                }
            }
            "K" => {
                if clean.is_empty() || clean == "0" {
                    self.rows[self.cr].truncate(self.cc);
                } else if clean == "2" {
                    self.rows[self.cr].clear();
                    self.cc = 0;
                }
            }
            // IL/DL: insert/delete n lines at the cursor row (scroll region).
            "L" => {
                for _ in 0..n {
                    self.rows.insert(self.cr, Vec::new());
                    self.rows.pop();
                }
            }
            "M" => {
                for _ in 0..n {
                    if self.cr < self.rows.len() {
                        self.rows.remove(self.cr);
                        self.rows.push(Vec::new());
                    }
                }
            }
            _ => {}
        }
        if command != "C" && command != "D" {
            // Most cursor moves cancel the deferred wrap state in real
            // terminals (any explicit positioning does).
            self.pending_wrap = false;
        }
    }

    fn dump(&self) -> String {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, r)| format!("{i:3}|{}", r.iter().collect::<String>()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn visible_text(&self) -> String {
        self.rows
            .iter()
            .map(|r| r.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The reported bug: typing into the editor once the transcript already fills
/// the screen. Each wrap grows the frame by one line at the bottom. The screen
/// must keep tracking the editor — a freeze means the visible text stops
/// matching the model text.
#[test]
fn debug_diff_editor_growth_past_viewport() {
    let width = 30u16;
    let height = 10u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);

    let marker = "\x1b_pi:c\x07";
    // Transcript fills the screen: 7 items + editor + footer = 9 rows at
    // start; editor growth pushes it past the viewport.
    let items: Vec<String> = (1..=7)
        .map(|i| format!("{i}. transcript line {i}"))
        .collect();
    let frame = |editor_visual: &[String]| -> Vec<Line> {
        let mut lines: Vec<Line> = items.iter().map(|s| Line::plain(s.clone())).collect();
        for (i, l) in editor_visual.iter().enumerate() {
            if i == 0 {
                lines.push(Line::plain(format!("> {l}{marker}")));
            } else {
                lines.push(Line::plain(format!("  {l}")));
            }
        }
        lines.push(Line::plain("footer ───────────"));
        lines
    };

    // Type 90 chars; editor wraps every 27 chars (30 - 2 gutter - 1 space).
    let mut text = String::new();
    for i in 0..90usize {
        text.push(char::from(b'a' + (i % 26) as u8));
        // Wrap the editor text like the editor component does (hard break).
        let avail = 27usize;
        let mut visual: Vec<String> = Vec::new();
        let mut rest = text.as_str();
        while rest.len() > avail {
            visual.push(rest[..avail].to_string());
            rest = &rest[avail..];
        }
        visual.push(rest.to_string());
        // Marker lives on the last visual line; move it there.
        let mut lines: Vec<Line> = items.iter().map(|s| Line::plain(s.clone())).collect();
        for (j, l) in visual.iter().enumerate() {
            if j + 1 == visual.len() {
                lines.push(Line::plain(format!(
                    "{}{l}{marker}",
                    if j == 0 { "> " } else { "  " }
                )));
            } else {
                lines.push(Line::plain(format!(
                    "{}{l}",
                    if j == 0 { "> " } else { "  " }
                )));
            }
        }
        lines.push(Line::plain("footer ───────────"));
        out.clear();
        renderer
            .render(&lines, width, height, false, &mut out)
            .unwrap();
        term.feed(&out);
        let _ = frame; // (closure kept for reference; inline build above)
    }
    let dump = term.dump();
    println!(
        "=== editor growth final ===\n{dump}\n(scrolled {} rows)",
        term.scrolled
    );
    let vis = term.visible_text();
    // The editor tail (last typed chars) and the footer must stay visible.
    let tail: String = text
        .chars()
        .rev()
        .take(10)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    assert!(vis.contains(&tail), "editor tail not visible:\n{dump}");
    assert_eq!(
        vis.matches("footer").count(),
        1,
        "footer wrong count:\n{dump}"
    );
}

/// Append past the viewport, then delete ONLY tail lines (no content change
/// above): the deletion path must clear the freed screen rows and leave the
/// surviving lines exactly once, without scrolling the viewport.
#[test]
fn debug_diff_tail_deletion_past_viewport() {
    let width = 30u16;
    let height = 10u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);

    let mut items: Vec<String> = Vec::new();
    for i in 1..=15usize {
        items.push(format!("{i}. line"));
        let lines: Vec<Line> = items.iter().map(|s| Line::plain(s.clone())).collect();
        out.clear();
        renderer
            .render(&lines, width, height, false, &mut out)
            .unwrap();
        term.feed(&out);
    }
    // Pure tail deletion: keep the first 8 lines.
    let lines: Vec<Line> = items[..8].iter().map(|s| Line::plain(s.clone())).collect();
    out.clear();
    renderer
        .render(&lines, width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    let dump = term.dump();
    println!("=== tail deletion ===\n{dump}");
    let vis = term.visible_text();
    // Viewport stays put: lines 6..=8 visible, everything below cleared.
    assert!(vis.contains("6. line"), "{dump}");
    assert!(vis.contains("7. line"), "{dump}");
    assert!(vis.contains("8. line"), "{dump}");
    assert!(!vis.contains("9. line"), "stale row survived:\n{dump}");
    assert!(!vis.contains("15. line"), "stale row survived:\n{dump}");
    for i in 1..=8usize {
        assert_eq!(
            vis.matches(&format!("{i}. line")).count(),
            if i >= 6 { 1 } else { 0 },
            "line {i} wrong count:\n{dump}"
        );
    }
}

/// Height shrink then grow (terminal resize): both paths full-re-render with
/// a screen clear; the screen must show exactly the frame tail each time.
#[test]
fn debug_diff_height_resize() {
    let width = 30u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();

    let items: Vec<String> = (1..=20usize).map(|i| format!("{i}. line")).collect();
    let lines: Vec<Line> = items.iter().map(|s| Line::plain(s.clone())).collect();

    let mut term = VirtualTerm::new(width as usize, 10);
    renderer.render(&lines, width, 10, false, &mut out).unwrap();
    term.feed(&out);

    // Shrink to 5 rows.
    out.clear();
    let mut small = VirtualTerm::new(width as usize, 5);
    renderer.render(&lines, width, 5, false, &mut out).unwrap();
    small.feed(&out);
    let vis = small.visible_text();
    println!("=== shrunk ===\n{}", small.dump());
    for i in 16..=20usize {
        assert!(vis.contains(&format!("{i}. line")), "missing {i}: {vis}");
    }
    assert!(!vis.contains("15. line"), "stale: {vis}");

    // Grow back to 10 rows.
    out.clear();
    let mut big = VirtualTerm::new(width as usize, 10);
    renderer.render(&lines, width, 10, false, &mut out).unwrap();
    big.feed(&out);
    let vis = big.visible_text();
    println!("=== regrown ===\n{}", big.dump());
    for i in 11..=20usize {
        assert_eq!(
            vis.matches(&format!("{i}. line")).count(),
            1,
            "line {i} wrong count:\n{vis}"
        );
    }
    assert!(!vis.contains("10. line"), "out of viewport: {vis}");
}

/// Width change: wrapping changes, so the renderer full-re-renders with a
/// clear; the new screen must show the frame tail exactly once.
#[test]
fn debug_diff_width_change() {
    let height = 10u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();

    let items: Vec<String> = (1..=12usize).map(|i| format!("{i}. line")).collect();
    let lines: Vec<Line> = items.iter().map(|s| Line::plain(s.clone())).collect();

    let mut term = VirtualTerm::new(30, height as usize);
    renderer
        .render(&lines, 30, height, false, &mut out)
        .unwrap();
    term.feed(&out);

    out.clear();
    let mut wide = VirtualTerm::new(60, height as usize);
    renderer
        .render(&lines, 60, height, false, &mut out)
        .unwrap();
    wide.feed(&out);
    let vis = wide.visible_text();
    println!("=== width change ===\n{}", wide.dump());
    for i in 3..=12usize {
        assert_eq!(
            vis.matches(&format!("{i}. line")).count(),
            1,
            "line {i} wrong count:\n{vis}"
        );
    }
}

/// Shrink the frame INSIDE the viewport (no scroll involved): changed lines
/// are rewritten and the deleted tail is cleared without touching rows above.
#[test]
fn debug_diff_shrink_inside_viewport() {
    let width = 30u16;
    let height = 30u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);

    let items: Vec<String> = (1..=20usize).map(|i| format!("{i}. line")).collect();
    let lines: Vec<Line> = items.iter().map(|s| Line::plain(s.clone())).collect();
    renderer
        .render(&lines, width, height, false, &mut out)
        .unwrap();
    term.feed(&out);

    // Keep 0..=2, rewrite line 3, drop the rest.
    let small = vec![
        Line::plain("1. line"),
        Line::plain("2. line"),
        Line::plain("3. line"),
        Line::plain("compact summary"),
        Line::plain("footer"),
    ];
    out.clear();
    renderer
        .render(&small, width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    let dump = term.dump();
    println!("=== shrink inside viewport ===\n{dump}");
    let vis = term.visible_text();
    assert_eq!(vis.matches("1. line").count(), 1, "{dump}");
    assert_eq!(vis.matches("compact summary").count(), 1, "{dump}");
    assert_eq!(vis.matches("footer").count(), 1, "{dump}");
    assert!(!vis.contains("5. line"), "stale row survived:\n{dump}");
    assert!(!vis.contains("20. line"), "stale row survived:\n{dump}");
}

/// The hardware cursor must land on the CURSOR_MARKER position after every
/// frame — including frames where the changed content is BELOW the marker.
#[test]
fn debug_diff_cursor_tracks_marker() {
    let width = 30u16;
    let height = 10u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);
    let marker = "\x1b_pi:c\x07";

    let frame = |tail: &str| -> Vec<Line> {
        vec![
            Line::plain("transcript"),
            Line::from_spans(vec![
                Span::plain("hello".to_string()),
                Span::raw(marker),
                Span::plain(" world".to_string()),
            ]),
            Line::plain(tail),
        ]
    };

    renderer
        .render(&frame("status-a"), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    assert_eq!((term.cr, term.cc), (1, 5), "{}", term.dump());

    // Change below the marker: cursor must come back to (1, 5).
    out.clear();
    renderer
        .render(&frame("status-b"), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    assert_eq!((term.cr, term.cc), (1, 5), "{}", term.dump());

    // Frame growth pushing the marker's row: marker row is absolute, cursor
    // follows the buffer row while it stays inside the viewport.
    out.clear();
    let mut grown: Vec<Line> = (0..20).map(|i| Line::plain(format!("row {i}"))).collect();
    grown.push(Line::from_spans(vec![
        Span::plain("ed".to_string()),
        Span::raw(marker),
    ]));
    renderer
        .render(&grown, width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    // 21 rows at height 10: buffer rows 11..=20 visible; marker row 20 →
    // screen row 9, col 2.
    assert_eq!((term.cr, term.cc), (9, 2), "{}", term.dump());
}

/// The user-reported scenario: a long streaming thinking block finishes
/// while its tail is on screen. The app keeps the already-printed block
/// expanded at MessageEnd (`thinking_printed`) because folding it would
/// shrink the frame above the viewport — the terminal cannot unprint those
/// rows, so a fold forces either a blank band (erasing the visible thinking
/// tail) or pulled-back scrollback rows (visible duplicates).
///
/// The commit frame therefore matches the streaming frame: nothing is
/// rewritten, no blank band appears, and no thinking row is lost. As the
/// next turn streams, the rows scroll into the scrollback exactly once.
#[test]
fn debug_diff_printed_thinking_commit_keeps_rows() {
    let width = 40u16;
    let height = 12u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);

    let hist: Vec<String> = (1..=6).map(|i| format!("hist {i}")).collect();
    let text_rows = ["alpha text row", "beta text row", "gamma text row"];
    let frame = |thinking_rows: Option<usize>| -> Vec<Line> {
        let mut lines: Vec<Line> = hist.iter().map(|s| Line::plain(s.clone())).collect();
        match thinking_rows {
            Some(n) => {
                for i in 0..n {
                    lines.push(Line::plain(format!("thinking row {i}")));
                }
            }
            None => lines.push(Line::plain("◦ Thought for a while (999 chars)")),
        }
        for t in text_rows {
            lines.push(Line::plain(t));
        }
        lines.push(Line::plain("⠋ Working…"));
        lines.push(Line::plain("> "));
        lines.push(Line::plain("footer"));
        lines
    };

    let screen_rows = |term: &VirtualTerm| -> Vec<String> {
        term.rows
            .iter()
            .map(|r| r.iter().collect::<String>().trim_end().to_string())
            .collect()
    };
    let assert_clean = |term: &VirtualTerm, tag: &str| {
        let rows = screen_rows(term);
        assert!(
            !rows.iter().any(|r| r.is_empty()),
            "blank band at {tag}: {rows:?}"
        );
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        for r in term
            .scrollback
            .iter()
            .cloned()
            .chain(rows.iter().cloned())
            .filter(|r| !r.is_empty())
        {
            *seen.entry(r).or_default() += 1;
        }
        let dups: Vec<(_, _)> = seen.iter().filter(|(_, n)| **n > 1).collect();
        assert!(dups.is_empty(), "duplicated rows at {tag}: {dups:?}");
    };

    // Phase 1: stream the thinking block + text (rows scroll into
    // scrollback; the thinking tail is on screen).
    for k in 0..=25usize {
        out.clear();
        renderer
            .render(&frame(Some(k)), width, height, false, &mut out)
            .unwrap();
        term.feed(&out);
    }
    assert!(term.scrolled > 0, "expected streaming to scroll");
    let visible_thinking_before = screen_rows(&term)
        .iter()
        .filter(|r| r.starts_with("thinking row"))
        .count();
    assert!(visible_thinking_before > 0, "{}", term.dump());

    // Phase 2: MessageEnd with the printed thinking kept expanded — the
    // committed entry renders exactly the streaming rows.
    out.clear();
    renderer
        .render(&frame(Some(25)), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    assert!(
        out.is_empty(),
        "printed thinking commit repainted: {:?}",
        String::from_utf8_lossy(&out)
    );
    let vis = term.visible_text();
    assert_eq!(
        vis.matches("thinking row").count(),
        visible_thinking_before,
        "visible thinking rows lost: {}",
        term.dump()
    );
    assert!(!vis.contains("Thought for a while"), "{vis}");
    let rows = screen_rows(&term);
    assert!(!rows.iter().any(|r| r.is_empty()), "blank band: {rows:?}");
    assert_clean(&term, "commit");

    // Phase 3: the next turn streams; the thinking rows scroll into the
    // scrollback exactly once and nothing is lost or duplicated.
    for k in 0..10usize {
        let mut f = frame(Some(25));
        for i in 0..k {
            f.insert(f.len() - 3, Line::plain(format!("turn2 row {i}")));
        }
        out.clear();
        renderer.render(&f, width, height, false, &mut out).unwrap();
        term.feed(&out);
    }
    let vis = term.visible_text();
    let all_rows: Vec<String> = term
        .scrollback
        .iter()
        .cloned()
        .chain(screen_rows(&term))
        .collect();
    for i in 0..25usize {
        let want = format!("thinking row {i}");
        assert_eq!(
            all_rows.iter().filter(|r| *r == &want).count(),
            1,
            "thinking row {i} lost or duplicated:\n{}",
            term.dump()
        );
    }
    for i in 0..9usize {
        assert!(
            vis.contains(&format!("turn2 row {i}"))
                || term
                    .scrollback
                    .iter()
                    .any(|r| r == &format!("turn2 row {i}")),
            "missing turn2 {i}: {vis}"
        );
    }
    assert_clean(&term, "after turn2");
}

/// A short thinking block that still fits inside the viewport folds
/// normally, in place: the label replaces it, rows below keep their screen
/// rows, and the scrollback is untouched (nothing above the viewport shrank).
#[test]
fn debug_diff_visible_thinking_folds_in_place() {
    let width = 40u16;
    let height = 12u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);

    let mut expanded = vec![Line::plain("hist 1"), Line::plain("hist 2")];
    for i in 0..3 {
        expanded.push(Line::plain(format!("thinking row {i}")));
    }
    expanded.push(Line::plain("alpha text"));
    expanded.push(Line::plain("⠋ Working…"));
    expanded.push(Line::plain("> "));
    expanded.push(Line::plain("footer"));
    renderer
        .render(&expanded, width, height, false, &mut out)
        .unwrap();
    term.feed(&out);

    let mut collapsed = vec![Line::plain("hist 1"), Line::plain("hist 2")];
    collapsed.push(Line::plain("◦ Thought for a while (300 chars)"));
    collapsed.push(Line::plain("alpha text"));
    collapsed.push(Line::plain("⠋ Working…"));
    collapsed.push(Line::plain("> "));
    collapsed.push(Line::plain("footer"));
    out.clear();
    renderer
        .render(&collapsed, width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("\x1b[2J"), "needless clear: {s:?}");
    assert!(!s.contains("\x1b[3J"), "scrollback wiped: {s:?}");
    let vis = term.visible_text();
    assert_eq!(vis.matches("Thought for a while").count(), 1, "{vis}");
    assert!(!vis.contains("thinking row"), "{vis}");
    assert_eq!(vis.matches("alpha text").count(), 1, "{vis}");
    assert_eq!(vis.matches("footer").count(), 1, "{vis}");
}

/// A straddling collapse that still reaches the renderer (ctrl+t on old
/// history while its tail is visible, compaction) clears the screen +
/// scrollback and reprints the frame: no blank band, no pulled-back row
/// that would later show up twice when scrolling back.
#[test]
fn debug_diff_straddling_collapse_clears_without_duplication() {
    let width = 40u16;
    let height = 12u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);

    let hist: Vec<String> = (1..=6).map(|i| format!("hist {i}")).collect();
    let text_rows = ["alpha text row", "beta text row", "gamma text row"];
    let frame = |thinking_rows: Option<usize>| -> Vec<Line> {
        let mut lines: Vec<Line> = hist.iter().map(|s| Line::plain(s.clone())).collect();
        match thinking_rows {
            Some(n) => {
                for i in 0..n {
                    lines.push(Line::plain(format!("thinking row {i}")));
                }
            }
            None => lines.push(Line::plain("◦ Thought for a while (999 chars)")),
        }
        for t in text_rows {
            lines.push(Line::plain(t));
        }
        lines.push(Line::plain("⠋ Working…"));
        lines.push(Line::plain("> "));
        lines.push(Line::plain("footer"));
        lines
    };
    for k in 0..=25usize {
        out.clear();
        renderer
            .render(&frame(Some(k)), width, height, false, &mut out)
            .unwrap();
        term.feed(&out);
    }
    out.clear();
    renderer
        .render(&frame(None), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\x1b[2J"), "screen not cleared: {s:?}");
    assert!(s.contains("\x1b[3J"), "scrollback not cleared: {s:?}");
    let vis = term.visible_text();
    assert_eq!(vis.matches("Thought for a while").count(), 1, "{vis}");
    assert_eq!(vis.matches("alpha text row").count(), 1, "{vis}");
    // No pulled-back row: the rewritten frame is the collapsed one only.
    assert!(!vis.contains("thinking row"), "{vis}");
    // The reprint may scroll its head into the (fresh) scrollback; every
    // surviving row must still appear exactly once.
    let screen: Vec<String> = term
        .rows
        .iter()
        .map(|r| r.iter().collect::<String>().trim_end().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    for r in term.scrollback.iter().chain(screen.iter()) {
        *seen.entry(r.clone()).or_default() += 1;
    }
    let dups: Vec<(_, _)> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert!(
        dups.is_empty(),
        "duplicated rows: {dups:?}\n{}",
        term.dump()
    );
}

/// A shrink whose removed rows all live above the viewport (the app folds a
/// "printed expanded" thinking block only after it has scrolled off) changes
/// nothing on screen: every row below the removed range keeps its content and
/// its screen row (the window top moves up by the same count). The renderer
/// must apply it as a scrollback-only edit — no repaint, no clear, no blank
/// band, no pulled-back row.
#[test]
fn debug_diff_scrollback_only_shrink_keeps_viewport() {
    let width = 30u16;
    let height = 10u16;
    let mut renderer = MainScreenRenderer::new();
    let mut out: Vec<u8> = Vec::new();
    let mut term = VirtualTerm::new(width as usize, height as usize);
    let frame = |start: usize, n: usize| -> Vec<Line> {
        (start..start + n)
            .map(|i| Line::plain(format!("row {i}")))
            .collect()
    };

    // 20 rows: the first 10 scroll into the scrollback, the viewport shows
    // rows 10..=19.
    out.clear();
    renderer
        .render(&frame(0, 20), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    let before = term.visible_text();
    assert!(before.contains("row 10"), "{}", term.dump());
    assert!(before.contains("row 19"), "{}", term.dump());

    // Fold: the first 5 rows disappear (all above the viewport). The visible
    // window must keep showing exactly the same content at the same rows.
    out.clear();
    renderer
        .render(&frame(5, 15), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    assert!(
        out.is_empty(),
        "scrollback-only shrink repainted: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        term.visible_text(),
        before,
        "viewport changed:\n{}",
        term.dump()
    );
    assert!(!term.scrollback.is_empty(), "scrollback lost");
    // Bookkeeping followed the fold: the next row appended lands right after
    // the (now shorter) frame instead of the stale position.
    out.clear();
    renderer
        .render(&frame(5, 16), width, height, false, &mut out)
        .unwrap();
    term.feed(&out);
    let dump = term.dump();
    let vis = term.visible_text();
    assert!(vis.contains("row 15"), "appended row misplaced:\n{dump}");
    assert_eq!(
        vis.matches("row 15").count(),
        1,
        "appended row duplicated:\n{dump}"
    );
}
