//! Render pipeline benchmark: large-transcript frame cost (Termux lag
//! investigation). Run with:
//!   cargo test -p tack-app --test render_perf -- --ignored --nocapture
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use tack_app::tui::chat::{ChatEntry, NoticeKind};
use tack_app::tui::{TuiApp, TuiOptions};

async fn test_app(cwd: &std::path::Path) -> TuiApp {
    let agent_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir.path()) };
    unsafe { std::env::set_var("TACK_OFFLINE", "1") };
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
    app.test_close_dialog();
    app
}

/// Fill the transcript with realistic content: mixed ASCII + CJK markdown,
/// roughly `entries` * 12 rendered lines at width 100.
fn fill_transcript(app: &mut TuiApp, entries: usize, overflow_line: bool) {
    let para = "构建通过（grep 无匹配导致 exit 1）。清理格式并追加两个新测试，\
                then run cargo test -p tack-ai --test codebuddy_tests to verify the \
                cross-process resume path works as expected.\n";
    for i in 0..entries {
        let mut text = format!("## Turn {i}\n\n");
        for _ in 0..10 {
            text.push_str(para);
        }
        if overflow_line && i == entries / 2 {
            // One pathological long line (untruncated source) mid-transcript.
            text.push_str(&"x".repeat(500));
            text.push('\n');
        }
        app.test_push_chat(ChatEntry::Markdown { text });
        app.test_push_chat(ChatEntry::Notice {
            text: format!("✓ cargo test -p tack-ai --test codebuddy_tests (turn {i})"),
            kind: NoticeKind::Info,
        });
    }
}

fn bench_frames(app: &mut TuiApp, label: &str, frames: usize) {
    let mut out: Vec<u8> = Vec::new();
    // Warm-up: first frame builds all caches (and is a full rewrite).
    app.render(&mut out).unwrap();
    eprintln!("[{label}] first frame (full rewrite): {} bytes", out.len());
    let start = Instant::now();
    let mut steady_bytes = 0usize;
    for _ in 0..frames {
        out.clear();
        app.render(&mut out).unwrap();
        steady_bytes += out.len();
    }
    let total = start.elapsed();
    eprintln!(
        "[{label}] {frames} steady-state frames: total {:?}, avg {:.2} ms/frame, avg {} bytes/frame",
        total,
        total.as_secs_f64() * 1000.0 / frames as f64,
        steady_bytes / frames
    );
    // The per-turn pathology: one assistant message lands (MessageEnd).
    // With a blanket line_cache.clear() the next frame re-renders the
    // whole transcript (new span Arcs defeat the renderer's pointer
    // fingerprints) → first_changed lands at the top → FULL rewrite.
    // Without it, only the appended item's lines go out.
    app.test_push_chat(ChatEntry::Markdown {
        text: "Done — appended one assistant message.".to_string(),
    });
    out.clear();
    app.render(&mut out).unwrap();
    eprintln!(
        "[{label}] frame after one appended message: {} bytes",
        out.len()
    );
}

#[tokio::test]
#[ignore = "benchmark"]
async fn render_perf_large_transcript() {
    let cwd = tempfile::tempdir().unwrap();

    // ~31% of a 1048k context ≈ 300k tokens ≈ 1.2MB text ≈ ~12-15k lines
    // at width 100. 1000 entries × ~13 lines ≈ 13k lines.
    let mut app = test_app(cwd.path()).await;
    app.test_resize(100, 40);
    fill_transcript(&mut app, 1000, false);
    bench_frames(&mut app, "13k lines, all fit", 50);

    let mut app = test_app(cwd.path()).await;
    app.test_resize(100, 40);
    fill_transcript(&mut app, 1000, true);
    bench_frames(&mut app, "13k lines, one overflowing", 50);

    // Small transcript (~10% context) for comparison.
    let mut app = test_app(cwd.path()).await;
    app.test_resize(100, 40);
    fill_transcript(&mut app, 300, false);
    bench_frames(&mut app, "4k lines, all fit", 50);
}

/// Streaming scenario: big transcript + active run with thinking + text
/// deltas. Measures bytes written PER FRAME — the "recent content keeps
/// refreshing while thinking" report. Healthy: tens/hundreds of bytes
/// (only the changed tail lines). Pathological: KBs-MBs (region rewrite).
#[tokio::test]
#[ignore = "benchmark"]
async fn render_perf_streaming_thinking() {
    let cwd = tempfile::tempdir().unwrap();
    let mut app = test_app(cwd.path()).await;
    app.test_resize(100, 40);
    fill_transcript(&mut app, 500, false); // ~6.5k lines
    let mut out: Vec<u8> = Vec::new();
    app.render(&mut out).unwrap();
    app.test_set_running(true);
    app.handle_agent_event(tack_agent_core::AgentEvent::AgentStart)
        .await;

    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut partial = tack_ai::AssistantMessage::pending(&model);
    partial.content.push(tack_ai::ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: None,
        redacted: None,
    });
    partial.content.push(tack_ai::ContentBlock::Text {
        text: String::new(),
        text_signature: None,
    });
    let thinking_chunk = "思考过程：逐步分析问题，检查代码路径，确认修复方案。";
    let text_chunk = "正在输出回答内容，逐句展开。";
    let mut sizes = Vec::new();
    for _ in 0..120 {
        if let tack_ai::ContentBlock::Thinking { thinking, .. } = &mut partial.content[0] {
            thinking.push_str(thinking_chunk);
        }
        if let tack_ai::ContentBlock::Text { text, .. } = &mut partial.content[1] {
            text.push_str(text_chunk);
        }
        let event = tack_agent_core::AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: thinking_chunk.to_string(),
                partial: partial.clone(),
            },
            message: tack_agent_core::AgentMessage::Assistant(partial.clone()),
        };
        app.handle_agent_event(event).await;
        out.clear();
        app.render(&mut out).unwrap();
        sizes.push(out.len());
    }
    sizes.sort_unstable();
    eprintln!(
        "streaming frames: {} frames, bytes min={} p50={} p90={} max={} total={}",
        sizes.len(),
        sizes[0],
        sizes[sizes.len() / 2],
        sizes[sizes.len() * 9 / 10],
        sizes[sizes.len() - 1],
        sizes.iter().sum::<usize>()
    );
}
