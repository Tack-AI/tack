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
        }
    }

    fn newline(&mut self) {
        self.pending_wrap = false;
        if self.cr == self.height - 1 {
            self.rows.remove(0);
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
