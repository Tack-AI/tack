//! Headless TUI tests: app construction, frame rendering, command dispatch.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;

use tack_app::tui::chat::NoticeKind;
use tack_app::tui::{TuiApp, TuiOptions};

async fn test_app(cwd: &std::path::Path) -> TuiApp {
    let agent_dir = tempfile::tempdir().unwrap();
    // Point the agent dir at the temp dir so nothing touches the real profile.
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    // Offline: the startup update check must never hit the network, even
    // when a parallel test's settings.json is read by mistake (the
    // TACK_AGENT_DIR race below).
    unsafe { std::env::set_var("TACK_OFFLINE", "1") };
    // An existing settings file suppresses the first-run wizard; the
    // update check and desktop notifications are disabled so tests never
    // touch the network or emit OSC escapes to the test console.
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"updateCheck": false, "notifications": false}"#,
    )
    .unwrap();
    // A stored credential makes the provider "available" for the /model picker.
    std::fs::write(
        agent_dir.path().join("auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "test-key"}}"#,
    )
    .unwrap();
    // Leak: the agent dir must outlive the app.
    std::mem::forget(agent_dir);

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut app = TuiApp::new(TuiOptions {
        model,
        auth: Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        thinking: None,
        cwd: cwd.to_path_buf(),
        continue_session: false,
        system_prompt: None,
        session_dir: None,
        flags: tack_app::cli_flags::CliFlags::default(),
    })
    .await
    .unwrap();
    // Parallel tests race on TACK_AGENT_DIR: a startup dialog (first-run
    // wizard, trust prompt) may still open and swallow routed input.
    app.test_close_dialog();
    app
}

#[tokio::test]
async fn initial_frame_has_banner_editor_footer() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("tack"), "frame: {frame:?}");
    assert!(frame.contains("k3"), "frame: {frame:?}");
    assert!(frame.contains("> "), "editor gutter missing: {frame:?}");
    assert!(
        frame.contains("[ask]"),
        "permission mode missing: {frame:?}"
    );
}

#[tokio::test]
async fn slash_commands_update_state() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;

    // /thinking with a direct level
    app.on_submit("/thinking high".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("Thinking: high"), "frame: {frame:?}");
    assert!(
        frame.contains("• high"),
        "footer thinking level missing: {frame:?}"
    );

    // /mode cycles the permission mode
    app.on_submit("/mode".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("acceptEdits"), "frame: {frame:?}");

    // /model with a fuzzy query
    app.on_submit("/model anthropic/claude-fable-5".to_string())
        .await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(
        frame.contains("model: anthropic/claude-fable-5"),
        "frame: {frame:?}"
    );

    // /name + /session
    app.on_submit("/name my-session".to_string()).await;
    app.on_submit("/session".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("Session"), "frame: {frame:?}");

    // Unknown command warns.
    app.on_submit("/nope".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("unknown command"), "frame: {frame:?}");
}

/// Regression: the TUI resolved the provider adapter once at startup, so
/// /model-switching to a model with a different api kind streamed the new
/// model through the startup adapter — the anthropic adapter then failed
/// with "No API key for provider: codebuddy" (the HTTP adapters name the
/// *model's* provider in that error). After the fix, a codebuddy model goes
/// through the CodeBuddy CLI adapter, which without the CLI installed fails
/// with "codebuddy CLI not found" instead.
#[tokio::test]
async fn model_switch_rebinds_provider_adapter() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await; // anthropic/k3 startup model

    // Switch to a CodeBuddy model (api codebuddy-stream). No CLI in the test
    // env, so resolve_model falls back to a bare model with the provider's
    // native api kind.
    app.apply_select(
        tack_app::tui::commands::SelectPurpose::Model,
        "codebuddy/some-model",
    )
    .await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(
        frame.contains("model: codebuddy/some-model"),
        "frame: {frame:?}"
    );

    // A prompt must now go through the CodeBuddy CLI adapter. The run is
    // async; the assistant error message is persisted to the session file.
    app.on_submit("hi".to_string()).await;
    let sessions_root = app.agent_dir().join("sessions");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut content = String::new();
    while std::time::Instant::now() < deadline {
        content.clear();
        if sessions_root.is_dir() {
            for entry in walk_jsonl(&sessions_root) {
                if let Ok(text) = std::fs::read_to_string(&entry) {
                    content.push_str(&text);
                }
            }
        }
        if content.contains("codebuddy CLI not found")
            || content.contains("codebuddy turn failed")
            || content.contains("Authentication required")
            || content.contains("No API key")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // The assertion target is that the error comes from the CodeBuddy
    // adapter (vs the startup adapter's "No API key"). Without the CLI
    // that's the CLI-missing error; with the CLI installed but not
    // logged in (dev machines) it's the CLI's real error text, surfaced
    // verbatim since the errors-array extraction fix.
    assert!(
        content.contains("codebuddy CLI not found")
            || content.contains("codebuddy turn failed")
            || content.contains("Authentication required"),
        "expected an error from the CodeBuddy CLI adapter, got: {content}"
    );
    assert!(
        !content.contains("No API key"),
        "stream went through the startup HTTP adapter: {content}"
    );
}

/// Recursively collect *.jsonl files under `dir`.
fn walk_jsonl(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_jsonl(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn bash_bang_command_runs_and_records() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.on_submit("!echo tui-bash-ok".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(
        frame.contains("tui-bash-ok"),
        "bash output missing: {frame:?}"
    );
}

#[tokio::test]
async fn unknown_notice_kinds_render() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.notice("warn test", NoticeKind::Warning);
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    assert!(String::from_utf8_lossy(&out).contains("warn test"));
}

#[tokio::test]
async fn model_cycling_and_html_export() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;

    // Ctrl+P cycles through the current provider's catalog (k3 is not in it,
    // so the first cycle lands on the first catalog model).
    app.cycle_model(1).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(
        frame.contains("model: anthropic/claude-"),
        "frame: {frame:?}"
    );

    // Backward cycle returns somewhere valid without panicking.
    app.cycle_model(-1).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();

    // HTML export of a session with a couple of messages.
    let mut session = tack_session::SessionManager::create(cwd.path(), None).unwrap();
    session
        .append_message(tack_agent_core::AgentMessage::user("hello export"))
        .unwrap();
    let target = cwd.path().join("export.html");
    tack_app::tui::commands::export_html(&session, &target).unwrap();
    let html = std::fs::read_to_string(&target).unwrap();
    assert!(html.contains("<!doctype html>"));
    assert!(html.contains("background:"));
    assert!(html.contains("hello export"), "{html}");
}

#[tokio::test]
async fn streaming_tool_args_rerender_header() {
    // Regression: running tool cards are render-cached on partial-output
    // length, so args streaming in (before any output exists) used to leave
    // the header stuck at the first partial args (e.g. `subagent {}`).
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    let mut out: Vec<u8> = Vec::new();

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    let tool_call = |args: serde_json::Value| tack_ai::ContentBlock::ToolCall {
        id: "t1".into(),
        name: "bash".into(),
        arguments: args,
        thought_signature: None,
        namespace: None,
    };

    // Args start empty and stream in across message updates.
    partial.content = vec![tool_call(serde_json::json!({}))];
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageUpdate {
        assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: String::new(),
            partial: partial.clone(),
        },
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    app.render(&mut out).unwrap();
    assert!(!String::from_utf8_lossy(&out).contains("cargo test"));

    partial.content = vec![tool_call(serde_json::json!({ "command": "cargo test" }))];
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageUpdate {
        assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: String::new(),
            partial: partial.clone(),
        },
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(
        frame.contains("cargo test"),
        "streamed args never reached the tool card header"
    );
}

#[tokio::test]
async fn agent_events_render_to_transcript() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    // The diff renderer only writes changed lines; accumulate across renders.
    let mut out: Vec<u8> = Vec::new();
    let rendered = |app: &mut TuiApp, out: &mut Vec<u8>| app.render(out).unwrap();

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::text("")];

    // Streaming: message start → delta → end with a tool call.
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    partial.content = vec![tack_ai::ContentBlock::text("Hello")];
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageUpdate {
        assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "Hello".into(),
            partial: partial.clone(),
        },
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    rendered(&mut app, &mut out);
    assert!(
        String::from_utf8_lossy(&out).contains("Hello"),
        "streaming text missing"
    );

    // Tool execution lifecycle renders a card.
    app.handle_agent_event(tack_agent_core::AgentEvent::ToolExecutionStart {
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        args: serde_json::json!({ "path": "a.rs" }),
    })
    .await;
    rendered(&mut app, &mut out);
    assert!(
        String::from_utf8_lossy(&out).contains("read a.rs"),
        "tool title missing"
    );
    app.handle_agent_event(tack_agent_core::AgentEvent::ToolExecutionEnd {
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        result: tack_agent_core::AgentToolResult::text("file content here"),
        is_error: false,
    })
    .await;
    rendered(&mut app, &mut out);
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("✓"), "done marker missing: {frame:?}");
    assert!(frame.contains("file content here"), "tool output missing");

    // Final message lands in the transcript; streaming state clears.
    partial.stop_reason = tack_ai::StopReason::Stop;
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageEnd {
        message: tack_agent_core::AgentMessage::Assistant(partial),
    })
    .await;
    rendered(&mut app, &mut out);
    assert!(String::from_utf8_lossy(&out).contains("Hello"));
}

#[tokio::test]
async fn autocomplete_selection_survives_key_release_events() {
    use tack_tui::{InputEvent, Key, KeyEvent, Modifiers};
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    // The diff renderer only emits changes; accumulate across renders.
    let mut out: Vec<u8> = Vec::new();

    // Type "/mo" → autocomplete with multiple matches.
    for c in ['/', 'm', 'o'] {
        app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Char(c))))
            .await;
    }
    app.render(&mut out).unwrap();
    let frame = String::from_utf8_lossy(&out);
    assert!(frame.contains("/model"), "autocomplete missing: {frame:?}");

    // Down arrow moves the selection to the second item ("/scoped-models" in
    // const order).
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Down)))
        .await;
    app.render(&mut out).unwrap();
    let selected = strip_ansi(&String::from_utf8_lossy(&out)).contains("→ /scoped-models");
    assert!(
        selected,
        "down arrow should select the second item:\n{}",
        strip_ansi(&String::from_utf8_lossy(&out))
    );

    // A Kitty-protocol key RELEASE must not reset the selection.
    let release = KeyEvent {
        key: Key::Down,
        modifiers: Modifiers::NONE,
        is_release: true,
    };
    app.handle_input(InputEvent::Key(release)).await;
    app.render(&mut out).unwrap();
    let still = strip_ansi(&String::from_utf8_lossy(&out)).contains("→ /scoped-models");
    assert!(
        still,
        "release event reset the selection:\n{}",
        strip_ansi(&String::from_utf8_lossy(&out))
    );
}

/// Strip ANSI escape sequences for content assertions.
fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() && ch != '[' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[tokio::test]
async fn mode_command_with_argument_sets_mode_directly() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.on_submit("/mode bypass".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = strip_ansi(&String::from_utf8_lossy(&out));
    assert!(frame.contains("Permission mode: bypass"), "{frame}");
    assert!(frame.contains("[bypass]"), "footer mode: {frame}");

    app.on_submit("/mode plan".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = strip_ansi(&String::from_utf8_lossy(&out));
    assert!(frame.contains("[plan]"), "{frame}");

    app.on_submit("/mode nope".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = strip_ansi(&String::from_utf8_lossy(&out));
    assert!(frame.contains("invalid mode"), "{frame}");
}

// ---------------------------------------------------------------------
// TUI freeze repro: typing past the right edge on a small terminal with
// the transcript already filling the screen (Termux scenario).
// ---------------------------------------------------------------------

/// Faithful fixed-size virtual terminal: width×height grid, deferred
/// autowrap, newline scrolls at the bottom row, cursor moves clamp.
struct VirtualTerm {
    rows: Vec<Vec<char>>,
    width: usize,
    height: usize,
    cr: usize,
    cc: usize,
    pending_wrap: bool,
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
        }
    }

    fn newline(&mut self) {
        self.pending_wrap = false;
        if self.cr == self.height - 1 {
            self.rows.remove(0);
            self.rows.push(Vec::new());
        } else {
            self.cr += 1;
        }
        self.cc = 0;
    }

    fn put_char(&mut self, c: char) {
        if self.pending_wrap {
            self.newline();
        }
        while self.rows[self.cr].len() < self.cc {
            self.rows[self.cr].push(' ');
        }
        self.rows[self.cr].truncate(self.cc);
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

#[tokio::test]
async fn typing_past_right_edge_keeps_screen_live() {
    use tack_tui::{InputEvent, Key, KeyEvent};
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(40, 12);

    // Fill the transcript so the frame already overflows the viewport.
    for i in 0..10 {
        app.notice(format!("filler line {i}"), NoticeKind::Info);
    }

    let mut term = VirtualTerm::new(40, 12);
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    term.feed(&out);

    // Type 150 chars, one keystroke at a time, rendering after each.
    for i in 0..150u32 {
        let ch = char::from(b'a' + (i % 26) as u8);
        app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Char(ch))))
            .await;
        out.clear();
        app.render(&mut out).unwrap();
        if String::from_utf8_lossy(&out).contains("\x1b_") {
            panic!(
                "CURSOR_MARKER leaked to terminal at keystroke {}: {:?}",
                i + 1,
                String::from_utf8_lossy(&out).replace('\x1b', "<ESC>")
            );
        }
        term.feed(&out);
        if i % 25 == 0 {
            println!("--- after {} keys ---\n{}", i + 1, term.dump());
        }
    }
    let dump = term.dump();
    println!("=== final ===\n{dump}");
    let vis = term.visible_text();
    let text = app.test_editor_text();
    let tail: String = text
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    assert!(
        vis.contains(&tail),
        "editor tail {tail:?} not visible:\n{dump}"
    );
}

#[tokio::test]
async fn model_picker_lists_only_configured_providers() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await; // auth.json carries anthropic

    // Switch to a catalog model first so the picker can mark it current.
    app.on_submit("/model anthropic/claude-fable-5".to_string())
        .await;
    app.on_submit("/model".to_string()).await;
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    let frame = strip_ansi(&String::from_utf8_lossy(&out));
    assert!(frame.contains("Select model"), "{frame}");
    // Anthropic has credentials → listed; openai does not → filtered out.
    assert!(frame.contains("[anthropic]"), "{frame}");
    assert!(!frame.contains("[openai]"), "{frame}");
    // TS hint when no scoped models are configured.
    assert!(
        frame.contains("Only showing models from configured providers"),
        "{frame}"
    );
    // Current model is sorted first and checkmarked; name footer present.
    assert!(frame.contains("claude-fable-5 [anthropic] ✓"), "{frame}");
    assert!(frame.contains("Model Name:"), "{frame}");
}

#[cfg(feature = "mermaid")]
#[tokio::test]
async fn mermaid_image_not_redrawn_while_working() {
    use tack_agent_core::AgentEvent;
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(60, 18);

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let text_with_mermaid = |tail: &str| {
        format!("Here is the diagram:\n\n```mermaid\nflowchart LR; A-->B-->C\n```\n\n{tail}")
    };

    // Working state + streaming message that already contains a complete
    // mermaid block, with text still growing below it (the real-world case).
    app.handle_agent_event(AgentEvent::AgentStart).await;
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::text(text_with_mermaid(""))];
    app.handle_agent_event(AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;

    let mut out: Vec<u8> = Vec::new();
    app.render(&mut out).unwrap();
    assert!(!out.is_empty());

    // Stream 8 deltas of trailing text; the image lines must not be rewritten.
    for i in 0..8 {
        partial.content = vec![tack_ai::ContentBlock::text(text_with_mermaid(
            &"and more trailing text ".repeat(i + 1),
        ))];
        app.handle_agent_event(AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: partial.clone(),
            },
            message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
        })
        .await;
        out.clear();
        app.render(&mut out).unwrap();
        let frame = String::from_utf8_lossy(&out);
        let redrawn = frame.matches('▀').count();
        assert_eq!(
            redrawn,
            0,
            "delta {i}: {redrawn} image half-blocks rewritten:\n{}",
            strip_ansi(&frame)
        );
    }

    // Spinner ticks with no content change: nothing but the spinner line.
    out.clear();
    app.render(&mut out).unwrap();
    let idle = String::from_utf8_lossy(&out);
    assert!(!idle.contains('▀'), "idle render rewrote the image");
}

#[cfg(feature = "mermaid")]
#[tokio::test]
async fn mermaid_image_not_redrawn_when_scrolled() {
    use tack_agent_core::AgentEvent;
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    // Small viewport: the frame overflows almost immediately → scroll paths.
    app.test_resize(50, 10);

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let text_with_mermaid = |tail: &str| {
        format!(
            "Intro paragraph that is long enough to wrap across several lines of the terminal for sure.\n\n```mermaid\nflowchart LR; A-->B-->C\n```\n\n{tail}"
        )
    };

    app.handle_agent_event(AgentEvent::AgentStart).await;
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::text(text_with_mermaid(""))];
    app.handle_agent_event(AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    let mut out: Vec<u8> = Vec::new();
    app.render(&mut out).unwrap();

    for i in 0..10 {
        partial.content = vec![tack_ai::ContentBlock::text(text_with_mermaid(
            &"trailing words that keep the message growing and growing ".repeat(i + 1),
        ))];
        app.handle_agent_event(AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: partial.clone(),
            },
            message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
        })
        .await;
        out.clear();
        app.render(&mut out).unwrap();
        let frame = String::from_utf8_lossy(&out);
        let redrawn = frame.matches('▀').count();
        assert_eq!(
            redrawn,
            0,
            "delta {i}: {redrawn} image half-blocks rewritten:\n{}",
            strip_ansi(&frame)
        );
    }
}

#[cfg(feature = "mermaid")]
#[tokio::test]
async fn tall_mermaid_image_not_redrawn_when_scrolled() {
    use tack_agent_core::AgentEvent;
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(50, 16);

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    // Tall diagram: with a ~50-cell render width this exceeds the viewport.
    let tall = "flowchart TB\n A-->B\n B-->C\n C-->D\n D-->E\n E-->F\n F-->G\n G-->H\n H-->I";
    let text = |tail: &str| format!("```mermaid\n{tall}\n```\n\n{tail}");

    app.handle_agent_event(AgentEvent::AgentStart).await;
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::text(text(""))];
    app.handle_agent_event(AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    let mut out: Vec<u8> = Vec::new();
    app.render(&mut out).unwrap();

    for i in 0..10 {
        partial.content = vec![tack_ai::ContentBlock::text(text(
            &"more text keeps streaming below ".repeat(i + 1),
        ))];
        app.handle_agent_event(AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: partial.clone(),
            },
            message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
        })
        .await;
        out.clear();
        app.render(&mut out).unwrap();
        let frame = String::from_utf8_lossy(&out);
        let redrawn = frame.matches('▀').count();
        assert_eq!(
            redrawn,
            0,
            "delta {i}: {redrawn} image half-blocks rewritten:\n{}",
            strip_ansi(&frame)
        );
    }
}

#[tokio::test]
async fn no_duplicate_status_or_blank_bands_when_tools_stream_in() {
    use tack_agent_core::AgentEvent;
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(60, 14);

    // Fill the transcript past the viewport, start a run (Working status).
    for i in 0..8 {
        app.notice(format!("filler line {i}"), NoticeKind::Info);
    }
    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    app.handle_agent_event(AgentEvent::AgentStart).await;
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::text("reading files".to_string())];
    app.handle_agent_event(AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;

    let mut term = VirtualTerm::new(60, 14);
    let mut out: Vec<u8> = Vec::new();
    app.render(&mut out).unwrap();
    term.feed(&out);

    // A tool card streams in (inserted above the status line), then spinner ticks.
    app.handle_agent_event(AgentEvent::ToolExecutionStart {
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        args: serde_json::json!({ "path": "a.rs" }),
    })
    .await;
    out.clear();
    app.render(&mut out).unwrap();
    term.feed(&out);
    app.handle_agent_event(AgentEvent::ToolExecutionEnd {
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        result: tack_agent_core::AgentToolResult::text("file content here"),
        is_error: false,
    })
    .await;
    out.clear();
    app.render(&mut out).unwrap();
    term.feed(&out);
    for _ in 0..3 {
        app.test_tick();
        out.clear();
        app.render(&mut out).unwrap();
        term.feed(&out);
    }

    let dump = term.dump();
    println!("=== tool stream final ===\n{dump}");
    let vis = term.visible_text();
    assert_eq!(
        vis.matches("Working").count(),
        1,
        "status duplicated:\n{dump}"
    );
    // No run of 3+ fully-blank rows inside the content.
    let blank_run = term
        .rows
        .iter()
        .map(|r| r.iter().collect::<String>().trim().is_empty())
        .fold((0, 0), |(max, cur), b| {
            if b {
                (max.max(cur + 1), cur + 1)
            } else {
                (max, 0)
            }
        });
    assert!(
        blank_run.0 < 3,
        "blank band of {} rows:\n{dump}",
        blank_run.0
    );
}

#[tokio::test]
async fn cursor_lands_on_editor_line_at_all_widths() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    for width in [10u16, 20, 40, 80] {
        app.test_resize(width, 14);
        let mut term = VirtualTerm::new(width as usize, 14);
        let mut out: Vec<u8> = Vec::new();
        app.render(&mut out).unwrap();
        term.feed(&out);
        // The cursor must sit right after the "> " gutter on the editor line.
        let (cr, cc) = (term.cr, term.cc);
        let editor_row = term
            .rows
            .iter()
            .position(|r| r.iter().collect::<String>().starts_with("> "));
        assert_eq!(
            editor_row,
            Some(cr),
            "width {width}: cursor row {cr} != editor row {editor_row:?}:\n{}",
            term.dump()
        );
        assert_eq!(
            cc,
            2,
            "width {width}: cursor col {cc} != 2:\n{}",
            term.dump()
        );
    }
}

/// Regression: `tack -r` must not create a new (empty) session file before
/// showing the picker — the fresh file used to pollute the list and sort
/// first. The app runs on an in-memory placeholder until a session is
/// picked; cancelling exits (TS selectSession parity).
fn fresh_agent_dir() -> std::path::PathBuf {
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    unsafe { std::env::set_var("TACK_OFFLINE", "1") };
    // No update check / desktop notifications in tests (hermetic).
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"updateCheck": false, "notifications": false}"#,
    )
    .unwrap();
    std::fs::write(
        agent_dir.path().join("auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "test-key"}}"#,
    )
    .unwrap();
    let p = agent_dir.path().to_path_buf();
    std::mem::forget(agent_dir);
    p
}

#[tokio::test]
async fn resume_flag_does_not_create_session_file() {
    let agent_dir = fresh_agent_dir();
    let cwd = tempfile::tempdir().unwrap();
    // One pre-existing session in this cwd's session dir.
    let session_dir = tack_session::default_session_dir(cwd.path(), &agent_dir);
    let existing =
        tack_session::SessionManager::create(cwd.path(), Some(session_dir.clone())).unwrap();
    let existing_file = existing.session_file().unwrap().to_path_buf();
    let files_before = std::fs::read_dir(&session_dir).unwrap().count();

    let model = tack_app::model::resolve_model("anthropic", Some("k3"), &agent_dir).unwrap();
    let _app = TuiApp::new(TuiOptions {
        model,
        auth: Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        thinking: None,
        cwd: cwd.path().to_path_buf(),
        continue_session: false,
        system_prompt: None,
        session_dir: None,
        flags: tack_app::cli_flags::CliFlags {
            resume: true,
            ..Default::default()
        },
    })
    .await
    .unwrap();

    // No new file created just to show the picker.
    let files_after = std::fs::read_dir(&session_dir).unwrap().count();
    assert_eq!(
        files_before, files_after,
        "resume must not create a session file"
    );
    assert!(existing_file.exists());
}

#[tokio::test]
async fn resume_command_reports_empty_and_opens_picker() {
    let _agent_dir = fresh_agent_dir();
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await; // creates its own session file
    // The app's own session exists -> picker opens.
    assert!(app.command_resume(), "sessions present -> picker opens");

    // Remove all session files: nothing to resume -> false + notice.
    // Use the dir the APP captured — parallel tests rewrite TACK_AGENT_DIR.
    let session_dir = tack_session::default_session_dir(cwd.path(), app.agent_dir());
    for entry in std::fs::read_dir(&session_dir).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    assert!(!app.command_resume(), "no sessions -> false");
}

// ------------------------------------------------------------------
// Input queue UX: visible echo, recall, send-now, multi-line editing
// ------------------------------------------------------------------

use tack_app::tui::AppEvent;
use tack_tui::input::{InputEvent, Key, KeyEvent, Modifiers};

fn key(key: Key) -> InputEvent {
    InputEvent::Key(KeyEvent::plain(key))
}

fn type_text(text: &str) -> Vec<InputEvent> {
    text.chars().map(|c| key(Key::Char(c))).collect()
}

fn frame_text(app: &mut TuiApp) -> String {
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    String::from_utf8_lossy(&out).to_string()
}

#[tokio::test]
async fn enter_while_running_queues_with_visible_echo() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_running(true);

    for event in type_text("hello agent") {
        app.handle_input(event).await;
    }
    app.handle_input(key(Key::Enter)).await;

    assert_eq!(app.test_queued_messages(), vec!["hello agent".to_string()]);
    assert_eq!(app.test_editor_text(), "");
    let frame = frame_text(&mut app);
    assert!(frame.contains("queued"), "queued echo missing: {frame:?}");
    assert!(
        frame.contains("hello agent"),
        "queued text missing: {frame:?}"
    );
}

#[tokio::test]
async fn queued_message_is_marked_delivered_on_injection() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_running(true);
    app.on_submit("hold this thought".to_string()).await;
    assert!(frame_text(&mut app).contains("queued"));

    // The agent loop picked it up: MessageStart carries the user message.
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::user("hold this thought"),
    })
    .await;
    let frame = frame_text(&mut app);
    assert!(!frame.contains("queued"), "still queued: {frame:?}");
    assert!(
        frame.contains("hold this thought"),
        "user entry missing: {frame:?}"
    );
}

#[tokio::test]
async fn alt_up_recalls_newest_queued_message_to_editor() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_running(true);
    app.on_submit("first".to_string()).await;
    app.on_submit("second".to_string()).await;
    assert_eq!(
        app.test_queued_messages(),
        vec!["first".to_string(), "second".to_string()]
    );

    // Alt+Up recalls the NEWEST queued message for editing.
    app.handle_input(InputEvent::Key(KeyEvent::new(Key::Up, Modifiers::ALT)))
        .await;
    assert_eq!(app.test_editor_text(), "second");
    assert_eq!(app.test_queued_messages(), vec!["first".to_string()]);
    let frame = frame_text(&mut app);
    assert!(!frame.contains("second") || frame.matches("second").count() == 1);
    assert!(frame.contains("queued"), "first echo gone too: {frame:?}");

    // Recall again: queue empty, both lines in the editor.
    app.handle_input(InputEvent::Key(KeyEvent::new(Key::Up, Modifiers::ALT)))
        .await;
    assert_eq!(app.test_editor_text(), "second\nfirst");
    assert!(app.test_queued_messages().is_empty());
    assert!(!frame_text(&mut app).contains("queued"));
}

#[tokio::test]
async fn ctrl_r_history_search_filters_accepts_and_restores() {
    // ctrl+r opens an incremental reverse search through prompt history
    // (bash reverse-i-search); alt+↑ keeps the queue-recall binding.
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    // Seed editor history via real submissions (queued: a run is active).
    app.test_set_running(true);
    for text in ["alpha one", "beta two", "alpha three"] {
        for event in type_text(text) {
            app.handle_input(event).await;
        }
        app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Enter)))
            .await;
    }
    assert_eq!(app.test_editor_text(), "");

    // Ctrl+R: search mode opens with the usage hint in the frame.
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('r'))))
        .await;
    assert!(frame_text(&mut app).contains("reverse-i-search"));

    // Typing filters; the newest hit previews in the editor.
    for event in type_text("alpha") {
        app.handle_input(event).await;
    }
    assert_eq!(app.test_editor_text(), "alpha three");
    // Repeated ctrl+r cycles to the older hit; enter accepts (no submit).
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('r'))))
        .await;
    assert_eq!(app.test_editor_text(), "alpha one");
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Enter)))
        .await;
    assert_eq!(app.test_editor_text(), "alpha one");
    assert!(!frame_text(&mut app).contains("reverse-i-search"));

    // Clear the accepted text (ctrl+c empties a non-empty editor).
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('c'))))
        .await;
    assert_eq!(app.test_editor_text(), "");

    // Esc cancels and restores the pre-search draft.
    for event in type_text("draft") {
        app.handle_input(event).await;
    }
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('r'))))
        .await;
    for event in type_text("beta") {
        app.handle_input(event).await;
    }
    assert_eq!(app.test_editor_text(), "beta two");
    let frame = frame_text(&mut app);
    assert!(!frame.contains("no match"), "{frame:?}");
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Escape)))
        .await;
    assert_eq!(app.test_editor_text(), "draft");

    // No match: the bar says so and the draft preview stays.
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('r'))))
        .await;
    for event in type_text("zzz") {
        app.handle_input(event).await;
    }
    assert!(frame_text(&mut app).contains("no match"));
    assert_eq!(app.test_editor_text(), "draft");
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Escape)))
        .await;
}

#[tokio::test]
async fn ctrl_enter_while_running_aborts_and_sends_after_run() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_running(true);
    for event in type_text("urgent") {
        app.handle_input(event).await;
    }
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Enter)))
        .await;

    // Not queued: echoed as queued-for-feedback, editor cleared.
    assert!(app.test_queued_messages().is_empty());
    assert_eq!(app.test_editor_text(), "");
    assert!(frame_text(&mut app).contains("queued"));

    // The aborted run finishes: the message is submitted as a real prompt.
    app.test_app_event(AppEvent::RunFinished).await;
    let frame = frame_text(&mut app);
    assert!(!frame.contains("queued"), "echo not replaced: {frame:?}");
    assert!(frame.contains("urgent"), "prompt missing: {frame:?}");
    // The resubmission started a new run.
    app.test_app_event(AppEvent::RunFinished).await;
}

#[tokio::test]
async fn alt_enter_inserts_newline_instead_of_submitting() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    for event in type_text("line1") {
        app.handle_input(event).await;
    }
    app.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter, Modifiers::ALT)))
        .await;
    for event in type_text("line2") {
        app.handle_input(event).await;
    }
    assert_eq!(app.test_editor_text(), "line1\nline2");
}

#[tokio::test]
async fn multiline_paste_goes_into_editor_unsubmitted() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.handle_input(InputEvent::Paste("a\nb\nc".to_string()))
        .await;
    assert_eq!(app.test_editor_text(), "a\nb\nc");
    let frame = frame_text(&mut app);
    assert!(frame.contains('a') && frame.contains('c'));
}

#[tokio::test]
async fn ctrl_t_cycles_thinking_collapsed_expanded_hidden() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    // A completed assistant message with a thinking block.
    let model = tack_ai::Model {
        id: "mock".to_string(),
        name: "Mock".to_string(),
        api: "mock".to_string(),
        provider: "mock".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![tack_ai::InputKind::Text],
        cost: tack_ai::ModelCost::default(),
        context_window: 100_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let mut message = tack_ai::AssistantMessage::pending(&model);
    message.content = vec![
        tack_ai::ContentBlock::Thinking {
            thinking: "my secret reasoning".to_string(),
            thinking_signature: None,
            redacted: None,
        },
        tack_ai::ContentBlock::Text {
            text: "the answer".to_string(),
            text_signature: None,
        },
    ];
    message.stop_reason = tack_ai::StopReason::Stop;
    app.handle_agent_event(tack_agent_core::AgentEvent::MessageEnd {
        message: tack_agent_core::AgentMessage::Assistant(message),
    })
    .await;

    // Default: collapsed label, content hidden.
    let frame = frame_text(&mut app);
    assert!(frame.contains("Thought for a while"), "{frame:?}");
    assert!(!frame.contains("my secret reasoning"), "{frame:?}");

    // ctrl+t once: expanded in place.
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('t'))))
        .await;
    let frame = frame_text(&mut app);
    assert!(frame.contains("my secret reasoning"), "{frame:?}");

    // ctrl+t twice: hidden entirely.
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('t'))))
        .await;
    let frame = frame_text(&mut app);
    assert!(!frame.contains("my secret reasoning"), "{frame:?}");
    assert!(!frame.contains("Thought for a while"), "{frame:?}");

    // ctrl+t thrice: back to collapsed.
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('t'))))
        .await;
    let frame = frame_text(&mut app);
    assert!(frame.contains("Thought for a while"), "{frame:?}");
    assert!(!frame.contains("my secret reasoning"), "{frame:?}");
}

#[tokio::test]
async fn alt_up_recalls_pending_send_now_and_cancels_it() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_running(true);
    for event in type_text("urgent") {
        app.handle_input(event).await;
    }
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Enter)))
        .await;
    assert!(frame_text(&mut app).contains("queued"));

    // Recall the send-now echo before the abort lands: it must NOT fire.
    app.handle_input(InputEvent::Key(KeyEvent::new(Key::Up, Modifiers::ALT)))
        .await;
    assert_eq!(app.test_editor_text(), "urgent");
    app.test_app_event(AppEvent::RunFinished).await;
    assert_eq!(
        app.test_editor_text(),
        "urgent",
        "send-now must be cancelled"
    );
    assert!(app.test_queued_messages().is_empty());
    let frame = frame_text(&mut app);
    assert!(!frame.contains("queued"), "echo lingered: {frame:?}");
}

#[tokio::test]
async fn perf_frame_render_long_transcript() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(160, 50);

    // A long transcript: 400 entries with markdown-ish content.
    for i in 0..200 {
        app.on_submit(format!("question {i}")).await;
        app.test_app_event(AppEvent::RunFinished).await;
        // Each on_submit starts a run (mock provider fails async) — just
        // push transcript mass directly through notices instead.
    }
    let big_md = (0..200)
        .map(|i| format!("## Section {i}\n\nSome **bold** and `code` text with a [link](https://example.com).\n\n```rust\nfn f{i}() {{ let x = {i}; }}\n```\n"))
        .collect::<String>();
    app.run_command(&format!("echo {big_md}")).await;

    // Streaming partial: 50KB of markdown.
    let model = tack_ai::Model {
        id: "mock".into(),
        name: "Mock".into(),
        api: "mock".into(),
        provider: "mock".into(),
        base_url: "http://localhost".into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![tack_ai::InputKind::Text],
        cost: Default::default(),
        context_window: 100_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::Text {
        text: big_md.repeat(2),
        text_signature: None,
    }];

    // Warm up.
    let mut out = Vec::new();
    app.render(&mut out).unwrap();

    let start = std::time::Instant::now();
    const FRAMES: usize = 20;
    for _ in 0..FRAMES {
        let mut out = Vec::new();
        app.render(&mut out).unwrap();
    }
    let per_frame = start.elapsed() / FRAMES as u32;
    eprintln!("render: {per_frame:?}/frame");
    assert!(
        per_frame.as_millis() < 200,
        "frame render too slow: {per_frame:?}"
    );
}

fn bg_note(id: &str, command: &str, status: &str) -> tack_tools::background::TaskNotification {
    tack_tools::background::TaskNotification {
        task_id: id.into(),
        command: command.into(),
        status: status.into(),
    }
}

#[tokio::test]
async fn bg_notification_steers_running_agent() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_running(true);
    app.handle_bg_notification(bg_note("bg1", "cargo build", "finished"))
        .await;
    let queued = app.test_queued_messages();
    assert_eq!(queued.len(), 1, "expected one steering message");
    assert!(queued[0].contains("bg1"), "wake message: {}", queued[0]);
    assert!(queued[0].contains("cargo build"));
}

#[tokio::test]
async fn bg_notification_auto_wakes_idle_agent() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    assert!(!app.test_is_running());
    app.handle_bg_notification(bg_note("bg2", "sleep 540", "finished"))
        .await;
    assert!(app.test_is_running(), "idle agent was not woken");
}

#[tokio::test]
async fn bg_notification_stays_idle_when_auto_wake_disabled() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_set_background_auto_wake(false);
    app.handle_bg_notification(bg_note("bg3", "cargo test", "finished"))
        .await;
    assert!(
        !app.test_is_running(),
        "auto-wake disabled but a run started"
    );
    assert!(app.test_queued_messages().is_empty());
}

#[tokio::test]
async fn fullscreen_scroll_up_works_after_run_finishes() {
    use tack_agent_core::AgentEvent;
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(60, 12);
    app.toggle_fullscreen();

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    // Long content: much taller than the 12-row viewport.
    let long_text = (1..=40)
        .map(|i| format!("line {i}: assistant output that should remain scrollable"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content = vec![tack_ai::ContentBlock::text(long_text)];

    app.handle_agent_event(AgentEvent::AgentStart).await;
    app.handle_agent_event(AgentEvent::MessageStart {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    app.handle_agent_event(AgentEvent::MessageEnd {
        message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
    })
    .await;
    app.test_app_event(AppEvent::RunFinished).await;

    let before = frame_text(&mut app);
    // Termux touch scroll / mouse wheel up.
    for _ in 0..3 {
        app.handle_input(InputEvent::Mouse(tack_tui::MouseEvent {
            column: 10,
            row: 5,
            kind: tack_tui::MouseEventKind::ScrollUp,
            modifiers: Modifiers::NONE,
        }))
        .await;
    }
    let after = frame_text(&mut app);
    assert_ne!(
        strip_ansi(&before),
        strip_ansi(&after),
        "scrolling up after run end must change the frame:\n{}",
        strip_ansi(&after)
    );
    // The jump-to-bottom pill appears while scrolled up.
    assert!(
        strip_ansi(&after).contains("Back to bottom"),
        "pill missing:\n{}",
        strip_ansi(&after)
    );
    // End key jumps back to the bottom and resumes following.
    app.handle_input(key(Key::End)).await;
    let bottom = frame_text(&mut app);
    assert!(
        !strip_ansi(&bottom).contains("Back to bottom"),
        "pill should disappear at the bottom:\n{}",
        strip_ansi(&bottom)
    );
}

#[tokio::test]
async fn fullscreen_scroll_far_up_large_transcript_after_run() {
    use tack_agent_core::AgentEvent;
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(60, 12);
    app.toggle_fullscreen();

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    // Transcript far larger than the OVERSCAN window (64 lines).
    app.handle_agent_event(AgentEvent::AgentStart).await;
    for msg in 0..10 {
        let long_text = (1..=40)
            .map(|i| format!("msg{msg} line {i}: assistant output that stays scrollable"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut partial = tack_ai::AssistantMessage::pending(&model);
        partial.content = vec![tack_ai::ContentBlock::text(long_text)];
        app.handle_agent_event(AgentEvent::MessageStart {
            message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
        })
        .await;
        app.handle_agent_event(AgentEvent::MessageEnd {
            message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
        })
        .await;
    }
    app.test_app_event(AppEvent::RunFinished).await;
    // Materialize the scroll viewport once (the real loop renders before
    // any input is processed).
    let _ = frame_text(&mut app);

    // Scroll up many times: well past the overscan window.
    for _ in 0..60 {
        app.handle_input(InputEvent::Mouse(tack_tui::MouseEvent {
            column: 10,
            row: 5,
            kind: tack_tui::MouseEventKind::ScrollUp,
            modifiers: Modifiers::NONE,
        }))
        .await;
    }
    let after = frame_text(&mut app);
    let text = strip_ansi(&after);
    // 180 lines up from the bottom of a ~420-line transcript: middle
    // messages visible, no longer the tail.
    assert!(
        ["msg3 line", "msg4 line", "msg5 line", "msg6 line"]
            .iter()
            .any(|m| text.contains(m)),
        "scrolled far up, expected middle messages visible:\n{text}"
    );
    assert!(
        !text.contains("msg9 line 40"),
        "still pinned to the bottom after scrolling:\n{text}"
    );
    assert!(text.contains("Back to bottom"), "pill missing:\n{text}");
}

// ---------------------------------------------------------------------------
// tack-ext declarative widgets (v2.1) + autocomplete providers (v2.2)
// ---------------------------------------------------------------------------

#[cfg(feature = "ext")]
fn ext_widget(spec: serde_json::Value) -> tack_ext::WidgetSpec {
    serde_json::from_value(spec).unwrap()
}

/// Status segments render in the footer priority-ascending; empty text
/// hides the segment (rendering contract).
#[cfg(feature = "ext")]
#[tokio::test]
async fn ext_status_segments_render_sorted_and_hidden() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_register_ext_widget(
        "demo",
        ext_widget(serde_json::json!({"id": "z", "type": "status_line_segment",
            "priority": 50, "initial": {"text": "seg-z"}})),
    );
    app.test_register_ext_widget(
        "demo",
        ext_widget(serde_json::json!({"id": "a", "type": "status_line_segment",
            "priority": 10, "initial": {"text": "seg-a", "style": "info"}})),
    );
    app.test_register_ext_widget(
        "demo",
        ext_widget(serde_json::json!({"id": "h", "type": "status_line_segment",
            "priority": 1, "initial": {"text": "seg-hidden"}})),
    );
    // Hidden text starts empty; "seg-hidden" must never appear.
    app.test_app_event(tack_app::tui::AppEvent::ExtWidgetUpdate {
        plugin: "demo".to_string(),
        update: tack_ext::WidgetUpdatePayload {
            id: "h".to_string(),
            state: serde_json::json!({"text": ""}),
            visible: None,
        },
    })
    .await;

    let frame = strip_ansi(&frame_text(&mut app));
    let pos_a = frame.find("seg-a").expect("seg-a missing:\n{frame}");
    let pos_z = frame.find("seg-z").expect("seg-z missing:\n{frame}");
    assert!(pos_a < pos_z, "priority order violated:\n{frame}");
    assert!(
        !frame.contains("seg-hidden"),
        "empty text not hidden:\n{frame}"
    );
}

/// widget.update is a full-state replacement applied on the main loop;
/// unknown widget ids are tolerated; a dead plugin's widgets vanish.
#[cfg(feature = "ext")]
#[tokio::test]
async fn ext_widget_update_and_plugin_death() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_register_ext_widget(
        "demo",
        ext_widget(serde_json::json!({"id": "s", "type": "status_line_segment",
            "initial": {"text": "old-state"}})),
    );
    app.test_app_event(tack_app::tui::AppEvent::ExtWidgetUpdate {
        plugin: "demo".to_string(),
        update: tack_ext::WidgetUpdatePayload {
            id: "s".to_string(),
            state: serde_json::json!({"text": "new-state"}),
            visible: None,
        },
    })
    .await;
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(frame.contains("new-state"), "update not applied:\n{frame}");
    assert!(!frame.contains("old-state"), "state not replaced:\n{frame}");

    // Unknown widget id / unknown plugin: warn + ignore, no panic.
    app.test_app_event(tack_app::tui::AppEvent::ExtWidgetUpdate {
        plugin: "demo".to_string(),
        update: tack_ext::WidgetUpdatePayload {
            id: "nope".to_string(),
            state: serde_json::json!({"text": "x"}),
            visible: None,
        },
    })
    .await;
    app.test_app_event(tack_app::tui::AppEvent::ExtWidgetUpdate {
        plugin: "ghost".to_string(),
        update: tack_ext::WidgetUpdatePayload {
            id: "s".to_string(),
            state: serde_json::json!({"text": "x"}),
            visible: None,
        },
    })
    .await;

    // Plugin death removes all of its widgets (no residue).
    app.test_app_event(tack_app::tui::AppEvent::ExtPluginDead("demo".to_string()))
        .await;
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(
        !frame.contains("new-state"),
        "dead plugin's widget survived:\n{frame}"
    );
}

/// A list panel renders its items; alt+p focuses it, arrows navigate,
/// enter is consumed without a plugin (action dropped silently), and
/// ctrl+b hides the panels.
#[cfg(feature = "ext")]
#[tokio::test]
async fn ext_list_panel_focus_navigation_and_toggle() {
    use tack_tui::{InputEvent, Key, KeyEvent, Modifiers};
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_register_ext_widget(
        "demo",
        ext_widget(serde_json::json!({"id": "files", "type": "list_panel",
        "title": "Changed files", "visible": true,
        "initial": {"items": [
            {"id": "a", "label": "Alpha", "detail": "first"},
            {"id": "b", "label": "Beta"}
        ]}})),
    );
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(frame.contains("Changed files"), "title missing:\n{frame}");
    assert!(
        frame.contains("→ Alpha"),
        "initial selection missing:\n{frame}"
    );
    assert!(frame.contains("Beta"), "second item missing:\n{frame}");

    // Focus the panel, move the selection down.
    app.handle_input(InputEvent::Key(KeyEvent::new(
        Key::Char('p'),
        Modifiers::ALT,
    )))
    .await;
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Down)))
        .await;
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(frame.contains("→ Beta"), "selection did not move:\n{frame}");
    assert!(frame.contains("focused"), "focus marker missing:\n{frame}");

    // Enter is consumed (no plugin to answer; must not panic or submit).
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Enter)))
        .await;
    assert!(app.test_editor_text().is_empty());

    // Esc releases focus; ctrl+b hides all ext panels.
    app.handle_input(InputEvent::Key(KeyEvent::plain(Key::Escape)))
        .await;
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('b'))))
        .await;
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(
        !frame.contains("Changed files"),
        "panels not hidden by ctrl+b:\n{frame}"
    );
    app.handle_input(InputEvent::Key(KeyEvent::ctrl(Key::Char('b'))))
        .await;
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(
        frame.contains("Changed files"),
        "panels did not return:\n{frame}"
    );
}

/// A markdown panel renders through the pulldown-cmark pipeline and
/// widget.update replaces its content.
#[cfg(feature = "ext")]
#[tokio::test]
async fn ext_markdown_panel_renders_and_updates() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_register_ext_widget(
        "demo",
        ext_widget(serde_json::json!({"id": "notes", "type": "markdown_panel",
            "title": "Notes", "visible": true,
            "initial": {"markdown": "# Heading\nsome **bold** text"}})),
    );
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(frame.contains("Notes"), "panel title missing:\n{frame}");
    assert!(frame.contains("Heading"), "markdown not rendered:\n{frame}");

    app.test_app_event(tack_app::tui::AppEvent::ExtWidgetUpdate {
        plugin: "demo".to_string(),
        update: tack_ext::WidgetUpdatePayload {
            id: "notes".to_string(),
            state: serde_json::json!({"markdown": "replacement body"}),
            visible: None,
        },
    })
    .await;
    let frame = strip_ansi(&frame_text(&mut app));
    assert!(
        frame.contains("replacement body"),
        "markdown update not applied:\n{frame}"
    );
    assert!(
        !frame.contains("bold"),
        "old markdown survived the replacement:\n{frame}"
    );
}
