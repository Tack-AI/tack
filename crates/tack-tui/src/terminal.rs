//! Terminal substrate: crossterm raw mode + capability negotiation (Kitty
//! keyboard protocol, bracketed paste, mouse, focus events, synchronized
//! output) and terminal capability detection. Port of tack-tui's
//! `terminal.ts` / `terminal-image.ts` essentials.

use std::collections::VecDeque;
use std::io::Write;
use std::time::Duration;

use crossterm::event::{
    Event, EventStream, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use futures_util::StreamExt;

use crate::input::{InputEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};

/// Forced value for the inline-image capability (TS "terminal capability
/// overrides": `TACK_IMAGE_PROTOCOL=kitty|iterm2|none|auto`,
/// `terminal.images: "kitty"|"iterm2"|false|"auto"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageCapabilityOverride {
    Kitty,
    ITerm2,
    /// Images off entirely (`none` / `0` / `false`).
    Disabled,
}

/// Capability overrides applied after environment detection (port of
/// terminal-image.ts `TACK_HYPERLINKS` / `TACK_TRUE_COLOR` / `TACK_IMAGE_PROTOCOL`
/// env overrides plus coding-agent `setCapabilityOverrides`). `None` = `auto`
/// (keep the detected value). Precedence: settings > environment > detection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub hyperlinks: Option<bool>,
    pub true_color: Option<bool>,
    pub images: Option<ImageCapabilityOverride>,
}

impl CapabilityOverrides {
    pub fn is_empty(&self) -> bool {
        self.hyperlinks.is_none() && self.true_color.is_none() && self.images.is_none()
    }

    /// Parse a `1|0|auto` boolean override (TS parseBooleanCapabilityOverride):
    /// only `1` and `0` force a value; everything else is `auto`.
    pub fn parse_bool(value: &str) -> Option<bool> {
        match value {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }

    /// Parse an image-protocol override (`kitty|iterm2|none|0|auto`).
    pub fn parse_images(value: &str) -> Option<ImageCapabilityOverride> {
        match value.to_lowercase().as_str() {
            "kitty" => Some(ImageCapabilityOverride::Kitty),
            "iterm2" => Some(ImageCapabilityOverride::ITerm2),
            "none" | "0" => Some(ImageCapabilityOverride::Disabled),
            _ => None,
        }
    }

    /// Read the environment overrides: `TACK_HYPERLINKS`, `TACK_TRUE_COLOR`
    /// (`1|0|auto`) and `TACK_IMAGE_PROTOCOL` (`kitty|iterm2|none|auto`).
    pub fn from_env() -> Self {
        CapabilityOverrides {
            hyperlinks: std::env::var("TACK_HYPERLINKS")
                .ok()
                .and_then(|v| Self::parse_bool(&v)),
            true_color: std::env::var("TACK_TRUE_COLOR")
                .ok()
                .and_then(|v| Self::parse_bool(&v)),
            images: std::env::var("TACK_IMAGE_PROTOCOL")
                .ok()
                .and_then(|v| Self::parse_images(&v)),
        }
    }

    /// Force the overridden fields, leaving `auto` fields as detected.
    pub fn apply_to(&self, caps: &mut Capabilities) {
        if let Some(hyperlinks) = self.hyperlinks {
            caps.hyperlinks = hyperlinks;
        }
        if let Some(true_color) = self.true_color {
            caps.true_color = true_color;
        }
        match self.images {
            Some(ImageCapabilityOverride::Kitty) => {
                caps.kitty_images = true;
                caps.iterm2_images = false;
            }
            Some(ImageCapabilityOverride::ITerm2) => {
                caps.kitty_images = false;
                caps.iterm2_images = true;
            }
            Some(ImageCapabilityOverride::Disabled) => {
                caps.kitty_images = false;
                caps.iterm2_images = false;
            }
            None => {}
        }
    }
}

/// Settings-file overrides (TS `setCapabilityOverrides`): applied after the
/// env overrides, so settings take precedence.
static SETTINGS_OVERRIDES: std::sync::RwLock<CapabilityOverrides> =
    std::sync::RwLock::new(CapabilityOverrides {
        hyperlinks: None,
        true_color: None,
        images: None,
    });

/// Install settings-file capability overrides (called once at TUI startup).
pub fn set_capability_overrides(overrides: CapabilityOverrides) {
    if let Ok(mut guard) = SETTINGS_OVERRIDES.write() {
        *guard = overrides;
    }
}

/// The currently installed settings-file overrides (empty = all `auto`).
pub fn capability_overrides() -> CapabilityOverrides {
    SETTINGS_OVERRIDES
        .read()
        .map(|guard| *guard)
        .unwrap_or_default()
}

/// Detected terminal capabilities.
#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    /// Kitty keyboard protocol accepted (key release events, disambiguated
    /// modifiers).
    pub kitty_keyboard: bool,
    /// Kitty graphics protocol images.
    pub kitty_images: bool,
    /// iTerm2 inline images.
    pub iterm2_images: bool,
    /// 24-bit color.
    pub true_color: bool,
    /// OSC 8 hyperlinks.
    pub hyperlinks: bool,
    /// Inside tmux/screen (images disabled, escape timing longer).
    pub multiplexed: bool,
}

impl Capabilities {
    /// Detect from the environment, then apply the capability overrides:
    /// env vars (`TACK_HYPERLINKS` / `TACK_TRUE_COLOR` / `TACK_IMAGE_PROTOCOL`)
    /// first, settings-file overrides (set_capability_overrides) last —
    /// settings take precedence (TS detectCapabilities +
    /// setCapabilityOverrides).
    pub fn detect() -> Self {
        let mut caps = Self::detect_from_environment();
        CapabilityOverrides::from_env().apply_to(&mut caps);
        capability_overrides().apply_to(&mut caps);
        caps
    }

    /// Pure environment detection (no overrides applied).
    fn detect_from_environment() -> Self {
        let term_program = std::env::var("TERM_PROGRAM")
            .unwrap_or_default()
            .to_lowercase();
        let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
        let multiplexed = std::env::var("TMUX").is_ok() || term.starts_with("screen");

        // Kitty graphics only on terminals proven to honor the protocol.
        // Warp reports TERM_PROGRAM=WarpTerminal and claims Kitty support,
        // but at least Warp for Windows renders NOTHING for Kitty/iTerm2
        // transmissions (verified via `tack debug-image` probe) — keep it on
        // the half-block fallback; TACK_IMAGE_PROTOCOL=kitty overrides.
        let kitty_like = matches!(term_program.as_str(), "kitty" | "ghostty" | "wezterm")
            || term.contains("kitty");
        let iterm = term_program == "iterm.app";
        let modern = matches!(
            term_program.as_str(),
            "vscode" | "windows_terminal" | "wezterm" | "ghostty" | "warp"
        ) || std::env::var("WT_SESSION").is_ok();

        Capabilities {
            kitty_keyboard: kitty_like || modern,
            kitty_images: kitty_like && !multiplexed,
            iterm2_images: iterm && !multiplexed,
            true_color: true, // assume 24-bit on anything modern; SGR 38;2 degrades gracefully
            hyperlinks: !multiplexed || term_program.contains("tmux"),
            multiplexed,
        }
    }
}

/// RAII guard: restores the terminal on drop.
#[derive(Debug)]
pub struct TerminalGuard {
    kitty_pushed: bool,
    mouse_enabled: bool,
    alt_screen: bool,
}

impl TerminalGuard {
    /// Enter raw mode + bracketed paste + focus events; push Kitty keyboard
    /// enhancement flags when supported.
    pub fn enter(alt_screen: bool, mouse: bool) -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        let mut kitty_pushed = false;
        if Capabilities::detect().kitty_keyboard {
            // flags 7: disambiguate + event types (press/release) + alternates
            let result = crossterm::execute!(
                out,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            );
            kitty_pushed = result.is_ok();
        }
        crossterm::execute!(
            out,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableFocusChange,
        )?;
        if mouse {
            crossterm::execute!(out, crossterm::event::EnableMouseCapture)?;
        }
        if alt_screen {
            crossterm::execute!(out, crossterm::terminal::EnterAlternateScreen)?;
            // Autowrap off + synchronized output are emitted per-frame by the
            // alt-screen backend.
            out.write_all(b"\x1b[?7l")?;
        }
        out.flush()?;
        Ok(TerminalGuard {
            kitty_pushed,
            mouse_enabled: mouse,
            alt_screen,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Flush any frames still queued on the writer thread BEFORE the
        // mode-restore sequences, or queued diff bytes would land after
        // the terminal left the TUI state (garbled final screen).
        drain_frames();
        let mut out = std::io::stdout();
        if self.alt_screen {
            let _ = out.write_all(b"\x1b[?7h");
            let _ = crossterm::execute!(out, crossterm::terminal::LeaveAlternateScreen);
        }
        if self.mouse_enabled {
            let _ = crossterm::execute!(out, crossterm::event::DisableMouseCapture);
        }
        let _ = crossterm::execute!(
            out,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableFocusChange,
        );
        if self.kitty_pushed {
            let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = out.flush();
    }
}

/// Max gap between key events that still counts as one input burst. A
/// paste without bracketed-paste support arrives as back-to-back key
/// events (already buffered by crossterm's reader thread, ~0 ms apart),
/// while human keystrokes are >30 ms apart, so 10 ms never merges real
/// typing.
const BURST_GAP: Duration = Duration::from_millis(10);

/// Burst-collector state machine (separated from the crossterm stream so
/// the deadline semantics are unit-testable).
///
/// The deadline is an ABSOLUTE wall-clock point kept in the struct, not a
/// relative timeout living inside the `next()` future: `next()` is polled
/// inside `tokio::select!` and loses constantly to agent stream deltas
/// during a run, and every loss drops the in-future timer. A relative
/// window would restart on each poll — under a continuously-ready
/// competing branch the burst never finishes and typed input starves for
/// the whole run (Termux: keyboard dead while "Working…").
#[derive(Debug, Default)]
struct BurstCollector {
    /// Burst events gathered so far. Non-empty while a burst collection is
    /// in progress (across select! cancellations).
    events: Vec<InputEvent>,
    /// Last collected event + BURST_GAP; the burst flushes at this point
    /// even across cancellations.
    deadline: Option<std::time::Instant>,
    /// The non-burst event that ended the current burst (survives
    /// cancellation for the same reason).
    interrupted: Option<InputEvent>,
}

impl BurstCollector {
    fn is_active(&self) -> bool {
        !self.events.is_empty()
    }

    fn start(&mut self, first: InputEvent, now: std::time::Instant) {
        self.events.push(first);
        self.deadline = Some(now + BURST_GAP);
    }

    /// Remaining wait before the burst must flush (zero = flush now).
    fn wait(&self, now: std::time::Instant) -> Duration {
        self.deadline
            .map(|d| d.saturating_duration_since(now))
            .unwrap_or(BURST_GAP)
    }

    fn push(&mut self, event: InputEvent, now: std::time::Instant) {
        self.events.push(event);
        self.deadline = Some(now + BURST_GAP);
    }

    fn interrupt(&mut self, event: InputEvent) {
        self.interrupted = Some(event);
    }

    fn finish(&mut self) -> (InputEvent, Vec<InputEvent>) {
        self.deadline = None;
        finish_burst(std::mem::take(&mut self.events), self.interrupted.take())
    }
}

/// Terminal event source (crossterm → InputEvent).
///
/// `next()` is polled inside `tokio::select!` and loses every race except
/// the one that wins — i.e. it is cancelled constantly (100 ms tick, agent
/// stream deltas, …). All consumed-but-undelivered events therefore live in
/// `self` (`burst` / `interrupted` / `pending`), never in locals: dropping
/// the future mid-burst loses nothing and the next poll resumes where the
/// cancelled one stopped.
#[derive(Debug)]
pub struct TerminalEvents {
    stream: EventStream,
    /// In-progress burst collection (see [`BurstCollector`]; the absolute
    /// deadline is what keeps typed input from starving under a
    /// continuously-ready competing `select!` branch).
    burst: BurstCollector,
    /// Replayed events of finished bursts: leftovers of plain-typing
    /// bursts, then the interrupting event, in original stream order.
    pending: VecDeque<InputEvent>,
}

impl Default for TerminalEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalEvents {
    pub fn new() -> Self {
        TerminalEvents {
            stream: EventStream::new(),
            burst: BurstCollector::default(),
            pending: VecDeque::new(),
        }
    }

    pub async fn next(&mut self) -> Option<InputEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        if !self.burst.is_active() {
            let first = self.read_mapped().await?;
            if burst_char(&first).is_none() {
                return Some(first);
            }
            // Paste-burst coalescing: when the terminal delivers no
            // bracketed paste (Windows console, Termux, "paste as typing"
            // modes), a multi-line paste arrives as a rapid burst of key
            // events whose newlines the editor would read as Enter —
            // submitting one message per line. Merge any burst containing
            // newlines into one Paste.
            self.burst.start(first, std::time::Instant::now());
        }
        // Collecting a burst. Every consumed event is already in `self`,
        // so a select! cancellation here is harmless: the next call
        // re-enters this loop and keeps collecting. The wait is computed
        // from the STORED absolute deadline, so a cancelled poll does not
        // restart the window — the first poll after the deadline flushes.
        loop {
            let wait = self.burst.wait(std::time::Instant::now());
            if wait.is_zero() {
                break;
            }
            match tokio::time::timeout(wait, self.read_mapped()).await {
                // Drop key releases: the Windows console (and Kitty
                // keyboards) report a release for every press, which would
                // split every burst at its first character. All downstream
                // consumers ignore releases, so dropping them is safe.
                Ok(Some(event)) if is_release(&event) => {}
                Ok(Some(event)) if burst_char(&event).is_some() => {
                    self.burst.push(event, std::time::Instant::now());
                }
                Ok(Some(other)) => {
                    self.burst.interrupt(other);
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        let (head, replay) = self.burst.finish();
        self.pending.extend(replay);
        Some(head)
    }

    /// Non-blocking sibling of `next()`: returns an event only when one
    /// completes without waiting. The UI loop uses it to drain a backlog
    /// of queued input into a single frame (slow terminals render once per
    /// batch instead of once per keystroke). A burst interrupted
    /// mid-collect keeps its state — the same cancel-safety `next()`
    /// relies on under `select!`.
    pub async fn try_next(&mut self) -> Option<InputEvent> {
        tokio::time::timeout(Duration::ZERO, self.next())
            .await
            .unwrap_or_default()
    }

    async fn read_mapped(&mut self) -> Option<InputEvent> {
        loop {
            match self.stream.next().await {
                Some(Ok(event)) => {
                    if let Some(mapped) = map_event(event) {
                        // Input forensics: enable with
                        // `RUST_LOG=tack_tui::input=debug` when a terminal
                        // misbehaves (paste bursts, swallowed keys).
                        tracing::debug!(target: "tack_tui::input", event = ?mapped, "terminal input");
                        return Some(mapped);
                    }
                }
                Some(Err(_)) => return None,
                None => return None,
            }
        }
    }
}

/// Decide what a gathered burst becomes: a single Paste when it contains
/// newlines (multi-line paste), otherwise the keys replayed unchanged
/// (plain typing / single-line paste behave identically downstream).
fn coalesce_burst(burst: Vec<InputEvent>) -> (InputEvent, Vec<InputEvent>) {
    let has_newline = burst.iter().any(|e| matches!(burst_char(e), Some('\n')));
    // Rapid Enter taps on a laggy terminal gather into a newline-only burst.
    // Those are repeated confirms, not pasted text: collapsing them into a
    // Paste would swallow the confirm (dialogs ignore pastes entirely).
    let all_newlines = burst.iter().all(|e| matches!(burst_char(e), Some('\n')));
    if has_newline && burst.len() > 1 && !(all_newlines && burst.len() <= 4) {
        let text: String = burst.iter().filter_map(burst_char).collect();
        return (InputEvent::Paste(text), Vec::new());
    }
    let mut iter = burst.into_iter();
    let head = iter.next().expect("burst is non-empty");
    (head, iter.collect())
}

/// Key-release event? Windows console and Kitty-protocol keyboards emit a
/// release for every press.
fn is_release(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Key(key) if key.is_release)
}

/// Assemble the replay queue for a finished burst: the burst's own events
/// (or nothing, when it became a Paste) followed by the interrupting
/// event, preserving the original stream order.
fn finish_burst(
    burst: Vec<InputEvent>,
    interrupted: Option<InputEvent>,
) -> (InputEvent, Vec<InputEvent>) {
    let (head, mut replay) = coalesce_burst(burst);
    replay.extend(interrupted);
    (head, replay)
}

/// The text character a key event contributes to a paste burst, or `None`
/// for events that never originate from pasted text. Release events are
/// filtered by the caller.
fn burst_char(event: &InputEvent) -> Option<char> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.is_release {
        return None;
    };
    match key.key {
        // Windows console reports pasted uppercase as shift+char; unix
        // delivers the bare char. Ctrl/alt chars are real shortcuts —
        // except LF below.
        Key::Char(c) if !key.modifiers.ctrl && !key.modifiers.alt => Some(c),
        // Pasted LF in raw mode parses as ctrl+j (crossterm issue #371).
        Key::Char('j') if key.modifiers == Modifiers::CTRL => Some('\n'),
        // Pasted CR (unix paste bytes, Windows console Return key).
        Key::Enter if key.modifiers == Modifiers::NONE => Some('\n'),
        // Pasted tab byte (kept out of the autocomplete trigger).
        Key::Tab if key.modifiers == Modifiers::NONE => Some('\t'),
        _ => None,
    }
}

fn map_event(event: Event) -> Option<InputEvent> {
    match event {
        Event::Key(key) => {
            let modifiers = Modifiers {
                ctrl: key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL),
                alt: key.modifiers.contains(crossterm::event::KeyModifiers::ALT),
                shift: key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT),
            };
            let key_code = match key.code {
                crossterm::event::KeyCode::Char(c) => Key::Char(c),
                crossterm::event::KeyCode::Enter => Key::Enter,
                crossterm::event::KeyCode::Esc => Key::Escape,
                crossterm::event::KeyCode::Backspace => Key::Backspace,
                crossterm::event::KeyCode::Delete => Key::Delete,
                crossterm::event::KeyCode::Tab => Key::Tab,
                crossterm::event::KeyCode::BackTab => Key::BackTab,
                crossterm::event::KeyCode::Up => Key::Up,
                crossterm::event::KeyCode::Down => Key::Down,
                crossterm::event::KeyCode::Left => Key::Left,
                crossterm::event::KeyCode::Right => Key::Right,
                crossterm::event::KeyCode::Home => Key::Home,
                crossterm::event::KeyCode::End => Key::End,
                crossterm::event::KeyCode::PageUp => Key::PageUp,
                crossterm::event::KeyCode::PageDown => Key::PageDown,
                crossterm::event::KeyCode::Insert => Key::Insert,
                crossterm::event::KeyCode::F(n) => Key::F(n),
                _ => return None,
            };
            Some(InputEvent::Key(KeyEvent {
                key: key_code,
                modifiers,
                is_release: key.kind == KeyEventKind::Release,
            }))
        }
        Event::Paste(text) => Some(InputEvent::Paste(text)),
        Event::Mouse(mouse) => {
            use crossterm::event::MouseEventKind as K;
            let kind = match mouse.kind {
                K::Down(b) => MouseEventKind::Down(match b {
                    crossterm::event::MouseButton::Left => MouseButton::Left,
                    crossterm::event::MouseButton::Right => MouseButton::Right,
                    crossterm::event::MouseButton::Middle => MouseButton::Middle,
                }),
                K::Up(b) => MouseEventKind::Up(match b {
                    crossterm::event::MouseButton::Left => MouseButton::Left,
                    crossterm::event::MouseButton::Right => MouseButton::Right,
                    crossterm::event::MouseButton::Middle => MouseButton::Middle,
                }),
                K::Drag(b) => MouseEventKind::Drag(match b {
                    crossterm::event::MouseButton::Left => MouseButton::Left,
                    crossterm::event::MouseButton::Right => MouseButton::Right,
                    crossterm::event::MouseButton::Middle => MouseButton::Middle,
                }),
                K::ScrollUp => MouseEventKind::ScrollUp,
                K::ScrollDown => MouseEventKind::ScrollDown,
                K::ScrollLeft => MouseEventKind::ScrollLeft,
                K::ScrollRight => MouseEventKind::ScrollRight,
                K::Moved => return None,
            };
            Some(InputEvent::Mouse(MouseEvent {
                column: mouse.column,
                row: mouse.row,
                kind,
                modifiers: Modifiers {
                    ctrl: mouse
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL),
                    alt: mouse
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::ALT),
                    shift: mouse
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::SHIFT),
                },
            }))
        }
        Event::Resize(width, height) => Some(InputEvent::Resize { width, height }),
        Event::FocusGained => Some(InputEvent::FocusGained),
        Event::FocusLost => Some(InputEvent::FocusLost),
    }
}

/// Terminal size in cells.
pub fn size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Set the terminal window title (OSC 0). Control characters in the title
/// (ESC, BEL, other C0, DEL) are stripped — they could otherwise inject
/// escape sequences (e.g. via a session name).
pub fn set_title(title: &str) {
    let sanitized: String = title.chars().filter(|c| !c.is_control()).collect();
    let mut out = std::io::stdout();
    let _ = out.write_all(format!("\x1b]0;{sanitized}\x07").as_bytes());
    let _ = out.flush();
}

/// Strip characters that would break out of (or inject into) an OSC
/// sequence: control characters (ESC, BEL, other C0/C1, DEL) and the
/// OSC 777 `;` field separator.
fn sanitize_osc(text: &str) -> String {
    text.chars()
        .map(|c| if c == ';' { ':' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// Desktop notification, best effort: OSC 9 (iTerm2/kitty/WezTerm show
/// the body) plus OSC 777 `notify;title;body` (rxvt-unicode and others),
/// both BEL-terminated. Terminals without notification support ignore
/// the sequences — emitting them is harmless. No-op when stdout is not a
/// terminal (redirected output / headless tests).
pub fn notify(title: &str, body: &str) {
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        return;
    }
    let title = sanitize_osc(title);
    let body = sanitize_osc(body);
    let mut out = std::io::stdout();
    let _ =
        out.write_all(format!("\x1b]9;{body}\x07\x1b]777;notify;{title};{body}\x07").as_bytes());
    let _ = out.flush();
}

/// OSC 9;4 progress indicator (indeterminate); None clears.
pub fn set_progress(active: bool) {
    let mut out = std::io::stdout();
    let seq = if active {
        "\x1b]9;4;3\x07"
    } else {
        "\x1b]9;4;0\x07"
    };
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Bounded frame queue depth. At ~10–30 rendered frames per second, 16
/// queued frames absorb ~0.5–1.5 s of terminal stall (e.g. Termux chewing
/// through a full-transcript rewrite) without blocking the UI loop.
const FRAME_QUEUE_DEPTH: usize = 16;

/// Global frame queue + writer thread state.
struct FrameSink {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// Queued-but-not-yet-written buffers, for [`drain_frames`].
    in_flight: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
}

static FRAME_SINK: std::sync::OnceLock<FrameSink> = std::sync::OnceLock::new();

fn frame_sink() -> &'static FrameSink {
    FRAME_SINK.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        let in_flight =
            std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
        let counter = in_flight.clone();
        std::thread::Builder::new()
            .name("tack-tui-frame-writer".into())
            .spawn(move || {
                let stdout = std::io::stdout();
                while let Ok(buf) = rx.recv() {
                    {
                        let mut out = stdout.lock();
                        let _ = out.write_all(&buf);
                        let _ = out.flush();
                    }
                    let (lock, cvar) = &*counter;
                    let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
                    *n = n.saturating_sub(1);
                    drop(n);
                    cvar.notify_all();
                }
            })
            .expect("spawn tack-tui frame writer");
        FrameSink { tx, in_flight }
    })
}

/// Terminal frame sink that never blocks the UI loop on a slow terminal.
///
/// Each `write` call (one per renderer frame, plus small cursor/title
/// writes) is queued as a byte buffer to a writer thread that performs the
/// real — potentially blocking — pty writes. Node.js CLIs get this for
/// free (stdout is buffered and drained asynchronously); a synchronous
/// `write_all` on the UI loop is what froze input on Termux once the
/// transcript grew large and the terminal emulator fell behind.
///
/// Ordering is preserved (single FIFO). When the queue is full the send
/// blocks: the terminal is then truly stalled and dropping bytes would
/// corrupt the screen, so backpressure is the only safe fallback.
#[derive(Debug)]
pub struct FrameWriter;

impl std::io::Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let sink = frame_sink();
        {
            let (lock, _) = &*sink.in_flight;
            let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
        }
        if sink.tx.send(buf.to_vec()).is_err() {
            let (lock, cvar) = &*sink.in_flight;
            let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
            *n = n.saturating_sub(1);
            drop(n);
            cvar.notify_all();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "frame writer gone",
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // The writer thread flushes after every buffer; blocking here
        // would reintroduce the stall this type exists to remove.
        Ok(())
    }
}

/// Block until every queued frame has been written to the terminal. Called
/// before handing the terminal to another program (external editor,
/// suspend) and before restoring terminal modes on exit. No-op when the
/// writer thread never started.
///
/// Bounded: a terminal that stopped reading must not trap the caller
/// behind the stalled writer (on exit that would even suppress Ctrl+C —
/// raw mode is restored only after the drain). Past the deadline we
/// proceed with frames possibly still queued; the screen recovers on the
/// next redraw or after the terminal resumes.
pub fn drain_frames() {
    let Some(sink) = FRAME_SINK.get() else {
        return;
    };
    const DRAIN_FRAMES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let deadline = std::time::Instant::now() + DRAIN_FRAMES_TIMEOUT;
    let (lock, cvar) = &*sink.in_flight;
    let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
    while *n > 0 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let (guard, wait) = cvar
            .wait_timeout(n, remaining)
            .unwrap_or_else(|e| e.into_inner());
        n = guard;
        if wait.timed_out() {
            break;
        }
    }
}

/// Best-effort clipboard text read (native CLI per platform).
pub fn read_clipboard() -> Option<String> {
    let (cmd, args): (&str, &[&str]) = if cfg!(windows) {
        ("powershell", &["-NoProfile", "-Command", "Get-Clipboard"])
    } else if cfg!(target_os = "macos") {
        ("pbpaste", &[])
    } else {
        ("xclip", &["-selection", "clipboard", "-o"])
    };
    let output = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Runtime switch into the alternate screen (+ mouse capture).
pub fn enter_alt_screen(mouse: bool) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    crossterm::execute!(out, crossterm::terminal::EnterAlternateScreen)?;
    out.write_all(b"\x1b[?7l")?; // autowrap off
    if mouse {
        crossterm::execute!(out, crossterm::event::EnableMouseCapture)?;
    }
    out.flush()
}

/// Runtime switch back to the main screen.
pub fn leave_alt_screen(mouse: bool) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if mouse {
        crossterm::execute!(out, crossterm::event::DisableMouseCapture)?;
    }
    out.write_all(b"\x1b[?7h")?;
    crossterm::execute!(out, crossterm::terminal::LeaveAlternateScreen)?;
    out.flush()
}

/// Clipboard write: verified native backends with a gated OSC 52 fallback.
/// See the `clipboard` module (port of upstream pi `utils/clipboard.ts`).
/// Re-exported here so existing `terminal::copy_to_clipboard` callers keep
/// working.
pub use crate::clipboard::{ClipboardError, copy_to_clipboard};

/// Query the terminal's background color via OSC 11 (best effort, ~250ms
/// total timeout). Returns (r, g, b) in 0-255. Used by auto theme detection;
/// terminals that don't answer (most Windows consoles) return `None` and the
/// caller falls back to COLORFGBG/dark.
pub fn query_background_color() -> Option<(u8, u8, u8)> {
    use std::io::{IsTerminal, Read, Write};
    // Headless (redirected stdin/stdout): there is no terminal to answer,
    // and enable_raw_mode (tcsetattr) can BLOCK forever on exotic stdin
    // (Android/Termux sandboxes). Skip to the caller's env/dark fallback.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw && crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let result = (|| {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b]11;?\x1b\\").ok()?;
        out.flush().ok()?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        // Detached reader thread. On Unix it reads /dev/tty directly
        // rather than stdin: crossterm's own reader thread competes for
        // stdin bytes, and a loser of that race would swallow the
        // terminal's OSC 11 reply (or user keys). Elsewhere (or when the
        // tty cannot be opened) it falls back to stdin. Once `rx` is
        // dropped (query answered or timed out) the thread exits after at
        // most one more batch — the send fails and the loop breaks, so it
        // never blocks on read forever.
        std::thread::spawn(move || {
            let mut reader: Box<dyn Read + Send> = tty_reader();
            let mut buf = [0u8; 256];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut data = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .ok()?;
        while !data.windows(5).any(|w| w == b"\x1b]11;") || !is_osc11_complete(&data) {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(more) => data.extend_from_slice(&more),
                Err(_) => break,
            }
        }
        parse_osc11_response(&data)
    })();
    if !was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    result
}

/// Reader for the OSC 11 reply: `/dev/tty` on Unix (bypasses the stdin
/// competition with crossterm's reader thread), stdin as the fallback.
fn tty_reader() -> Box<dyn std::io::Read + Send> {
    #[cfg(unix)]
    if let Ok(tty) = std::fs::File::open("/dev/tty") {
        return Box::new(tty);
    }
    Box::new(std::io::stdin())
}

fn is_osc11_complete(data: &[u8]) -> bool {
    let Some(start) = data.windows(5).position(|w| w == b"\x1b]11;") else {
        return false;
    };
    data[start + 5..].iter().any(|&b| b == 0x07 || b == 0x1b)
}

/// Parse `\x1b]11;rgb:RR/GG/BB(BEL|ESC\]` — components are 1-4 hex digits.
fn parse_osc11_response(data: &[u8]) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(data);
    let start = text.find("\x1b]11;")? + 5;
    let rest = &text[start..];
    let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
    let rgb = rest[..end].strip_prefix("rgb:")?;
    let parse = |s: &str| {
        let s = s.trim();
        if s.is_empty() || s.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(s, 16).ok()?;
        let max = (1u32 << (s.len() * 4)) - 1;
        Some((value * 255 / max) as u8)
    };
    let mut parts = rgb.split('/');
    let r = parse(parts.next()?)?;
    let g = parse(parts.next()?)?;
    let b = parse(parts.next()?)?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::{
        Capabilities, CapabilityOverrides, ImageCapabilityOverride, burst_char, coalesce_burst,
        finish_burst, is_release, sanitize_osc,
    };
    use crate::input::{InputEvent, Key, KeyEvent, Modifiers};

    fn key(k: Key, modifiers: Modifiers) -> InputEvent {
        InputEvent::Key(KeyEvent::new(k, modifiers))
    }

    fn burst(text: &str) -> Vec<InputEvent> {
        text.chars()
            .map(|c| match c {
                '\n' => key(Key::Enter, Modifiers::NONE),
                c => key(Key::Char(c), Modifiers::NONE),
            })
            .collect()
    }

    #[test]
    fn burst_with_enters_becomes_one_paste() {
        let (head, rest) = coalesce_burst(burst("foo\nbar\nbaz"));
        assert_eq!(head, InputEvent::Paste("foo\nbar\nbaz".to_string()));
        assert!(rest.is_empty());
    }

    #[test]
    fn burst_without_enter_replays_keys() {
        // Fast typing or a single-line paste: no newline, so no risk of
        // spurious submits — replay the keys unchanged.
        let events = burst("hi");
        let (head, rest) = coalesce_burst(events);
        assert_eq!(head, key(Key::Char('h'), Modifiers::NONE));
        assert_eq!(rest, vec![key(Key::Char('i'), Modifiers::NONE)]);
    }

    #[test]
    fn lone_enter_is_not_a_paste() {
        let (head, rest) = coalesce_burst(burst("\n"));
        assert_eq!(head, key(Key::Enter, Modifiers::NONE));
        assert!(rest.is_empty());
    }

    #[test]
    fn rapid_enter_taps_replay_as_enters() {
        // Double/quad-tapping Enter on a laggy terminal gathers a
        // newline-only burst: those are repeated confirms (dialogs ignore
        // pastes, so coalescing would swallow the answer).
        for text in ["\n\n", "\n\n\n", "\n\n\n\n"] {
            let (head, rest) = coalesce_burst(burst(text));
            assert_eq!(head, key(Key::Enter, Modifiers::NONE), "{text:?}");
            assert_eq!(rest.len(), text.len() - 1, "{text:?}");
            assert!(
                rest.iter().all(|e| *e == key(Key::Enter, Modifiers::NONE)),
                "{text:?}"
            );
        }
        // Five+ newlines is a pasted blank block, not tapping.
        let (head, _) = coalesce_burst(burst("\n\n\n\n\n"));
        assert_eq!(head, InputEvent::Paste("\n\n\n\n\n".to_string()));
        // Newlines mixed with text remain a paste.
        let (head, _) = coalesce_burst(burst("\n\nfoo"));
        assert_eq!(head, InputEvent::Paste("\n\nfoo".to_string()));
    }

    #[test]
    fn ctrl_j_counts_as_pasted_newline() {
        // Raw mode parses a pasted LF byte as ctrl+j (crossterm #371).
        assert_eq!(
            burst_char(&key(Key::Char('j'), Modifiers::CTRL)),
            Some('\n')
        );
    }

    #[test]
    fn windows_shifted_and_tab_chars_join_burst() {
        // Windows console marks pasted uppercase with shift; tabs arrive
        // as Tab keys and must not reach the autocomplete trigger.
        assert_eq!(
            burst_char(&key(Key::Char('A'), Modifiers::SHIFT)),
            Some('A')
        );
        assert_eq!(burst_char(&key(Key::Tab, Modifiers::NONE)), Some('\t'));
    }

    #[test]
    fn real_shortcuts_break_burst() {
        for event in [
            key(Key::Char('c'), Modifiers::CTRL),
            key(Key::Char('j'), Modifiers::CTRL_SHIFT),
            key(Key::BackTab, Modifiers::SHIFT),
            key(Key::Up, Modifiers::NONE),
            InputEvent::Key(KeyEvent {
                key: Key::Char('a'),
                modifiers: Modifiers::NONE,
                is_release: true,
            }),
        ] {
            assert_eq!(burst_char(&event), None, "{event:?}");
        }
    }

    #[test]
    fn interrupted_event_replays_after_burst() {
        // IME autocorrect commits "text + Backspace" in one batch: the
        // Backspace must replay after the text lands, or it eats typed
        // characters (Termux regression).
        let backspace = key(Key::Backspace, Modifiers::NONE);
        let (head, replay) = finish_burst(burst("ab"), Some(backspace.clone()));
        assert_eq!(head, key(Key::Char('a'), Modifiers::NONE));
        assert_eq!(
            replay,
            vec![key(Key::Char('b'), Modifiers::NONE), backspace]
        );
    }

    #[test]
    fn interrupted_event_survives_paste_coalescing() {
        let up = key(Key::Up, Modifiers::NONE);
        let (head, replay) = finish_burst(burst("a\nb"), Some(up.clone()));
        assert_eq!(head, InputEvent::Paste("a\nb".to_string()));
        assert_eq!(replay, vec![up]);
    }

    #[test]
    fn key_releases_are_detected() {
        // Windows console reports a release per press; the burst loop
        // drops them so they can't split a paste at its first character.
        let release = InputEvent::Key(KeyEvent {
            key: Key::Char('a'),
            modifiers: Modifiers::NONE,
            is_release: true,
        });
        assert!(is_release(&release));
        assert!(!is_release(&key(Key::Char('a'), Modifiers::NONE)));
    }

    #[test]
    fn burst_deadline_survives_cancelled_polls() {
        // The Termux "keyboard dead while Working" starvation: next() is
        // cancelled mid-wait by a continuously-ready select! branch
        // (agent stream deltas). Each re-poll must keep approaching the
        // SAME absolute deadline — a relative window would restart and
        // the burst would never flush for the whole run.
        let mut collector = super::BurstCollector::default();
        let t0 = std::time::Instant::now();
        collector.start(key(Key::Char('a'), Modifiers::NONE), t0);
        assert!(collector.is_active());
        // Re-polled (after cancellation) 5 ms later: ~5 ms remain, and
        // crucially NOT a fresh 10 ms window.
        let wait = collector.wait(t0 + std::time::Duration::from_millis(5));
        assert!(wait <= std::time::Duration::from_millis(5), "{wait:?}");
        // Past the deadline: flush immediately even though no new event
        // arrived and the previous poll was cancelled mid-wait.
        assert!(
            collector
                .wait(t0 + std::time::Duration::from_millis(11))
                .is_zero()
        );
        let (head, rest) = collector.finish();
        assert_eq!(head, key(Key::Char('a'), Modifiers::NONE));
        assert!(rest.is_empty());
        assert!(!collector.is_active());
    }

    #[test]
    fn frame_writer_drains_without_deadlock() {
        // Empty buffers keep the test console clean; the queue/counter
        // logic is what's under test (a missed decrement would hang
        // drain_frames forever).
        use std::io::Write as _;
        let mut out = super::FrameWriter;
        for _ in 0..(super::FRAME_QUEUE_DEPTH * 2) {
            assert!(out.write_all(b"").is_ok());
        }
        super::drain_frames();
        assert!(out.write_all(b"").is_ok());
        super::drain_frames();
    }

    #[test]
    fn parse_bool_override_only_1_and_0_force() {
        assert_eq!(CapabilityOverrides::parse_bool("1"), Some(true));
        assert_eq!(CapabilityOverrides::parse_bool("0"), Some(false));
        // "auto", unset-style values and typos all keep detection.
        for value in ["auto", "", "true", "yes", "2"] {
            assert_eq!(CapabilityOverrides::parse_bool(value), None, "{value}");
        }
    }

    #[test]
    fn parse_images_override() {
        assert_eq!(
            CapabilityOverrides::parse_images("kitty"),
            Some(ImageCapabilityOverride::Kitty)
        );
        assert_eq!(
            CapabilityOverrides::parse_images("ITERM2"),
            Some(ImageCapabilityOverride::ITerm2)
        );
        assert_eq!(
            CapabilityOverrides::parse_images("none"),
            Some(ImageCapabilityOverride::Disabled)
        );
        assert_eq!(
            CapabilityOverrides::parse_images("0"),
            Some(ImageCapabilityOverride::Disabled)
        );
        for value in ["auto", "", "half-block", "sixel"] {
            assert_eq!(CapabilityOverrides::parse_images(value), None, "{value}");
        }
    }

    #[test]
    fn apply_to_forces_only_set_fields() {
        let detected = Capabilities {
            kitty_images: false,
            iterm2_images: true,
            true_color: false,
            hyperlinks: true,
            ..Default::default()
        };
        // Empty overrides (all "auto") preserve detection.
        let mut caps = detected;
        CapabilityOverrides::default().apply_to(&mut caps);
        assert!(caps.iterm2_images && !caps.kitty_images);
        assert!(!caps.true_color && caps.hyperlinks);

        let overrides = CapabilityOverrides {
            hyperlinks: Some(false),
            true_color: Some(true),
            images: Some(ImageCapabilityOverride::Kitty),
        };
        let mut caps = detected;
        overrides.apply_to(&mut caps);
        assert!(!caps.hyperlinks);
        assert!(caps.true_color);
        assert!(caps.kitty_images && !caps.iterm2_images);

        // Disabled turns every image protocol off.
        let mut caps = detected;
        CapabilityOverrides {
            images: Some(ImageCapabilityOverride::Disabled),
            ..Default::default()
        }
        .apply_to(&mut caps);
        assert!(!caps.kitty_images && !caps.iterm2_images);
    }

    #[test]
    fn settings_overrides_win_over_env_overrides() {
        // Mirrors Capabilities::detect(): env applied first, settings last.
        let env = CapabilityOverrides {
            hyperlinks: Some(true),
            true_color: Some(false),
            images: Some(ImageCapabilityOverride::ITerm2),
        };
        let settings = CapabilityOverrides {
            hyperlinks: Some(false),
            true_color: None, // "auto": env value survives
            images: Some(ImageCapabilityOverride::Disabled),
        };
        let mut caps = Capabilities::default();
        env.apply_to(&mut caps);
        settings.apply_to(&mut caps);
        assert!(!caps.hyperlinks, "settings beat env");
        assert!(!caps.true_color, "auto in settings keeps the env override");
        assert!(!caps.kitty_images && !caps.iterm2_images);
    }

    #[test]
    fn notify_payload_is_sanitized() {
        // Control chars could terminate/inject OSC sequences; ';' would
        // shift the OSC 777 fields.
        assert_eq!(
            sanitize_osc("ti\u{1b}tle\u{7};x\ny"),
            "title:xy",
            "controls stripped, ';' replaced"
        );
        assert_eq!(sanitize_osc("clean title"), "clean title");
    }

    #[test]
    fn set_title_strips_control_characters() {
        // The sanitized title must contain no ESC/BEL/C0/DEL chars even when
        // the input (e.g. a session name) tries to inject escape sequences.
        let evil = "hi\u{1b}]0;pwned\u{7}\u{1b}[2J\u{7f}";
        let sanitized: String = evil.chars().filter(|c| !c.is_control()).collect();
        assert_eq!(sanitized, "hi]0;pwned[2J");
        assert!(!sanitized.chars().any(|c| c.is_control()));
    }
}
