//! Session export / share / browser-login helpers (extracted from
//! `commands.rs`; pure move, no behavior change).

use tack_session::SessionManager;

/// Browser-only OAuth login for the TUI (no stdin paste). Uses the flow's
/// fixed-port loopback callback.
pub(crate) async fn run_browser_login(
    agent_dir: &std::path::Path,
    provider: &str,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let flow = tack_ai::oauth::oauth_flow(provider)
        .with_context(|| format!("no OAuth flow for {provider}"))?;
    let client = reqwest::Client::new();
    let Some(browser) = flow
        .start_browser(&client, &Default::default())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
    else {
        anyhow::bail!(
            "{provider} is device-only; use `tack login --provider {provider} --device-code`"
        );
    };
    crate::oauth_login::open_browser(&browser.authorize_url);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", browser.callback_port))
        .await
        .context("cannot bind OAuth callback port")?;
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        crate::oauth_login::await_callback(listener, &browser.callback_path, &browser.state),
    )
    .await
    .context("login timed out")??;
    let credential = flow
        .exchange_code(&client, &browser, &code)
        .await
        .map_err(|e| anyhow::anyhow!("code exchange failed: {e}"))?;
    crate::auth::set_oauth(agent_dir, provider, &credential)?;
    Ok(())
}

/// POST the session to a secret GitHub gist; returns the HTML URL.
pub(crate) async fn share_via_gist(
    token: &str,
    name: &str,
    content: &str,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let response = client
        .post("https://api.github.com/gists")
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", concat!("tack/", env!("CARGO_PKG_VERSION")))
        .header("accept", "application/vnd.github+json")
        .json(&serde_json::json!({
            "public": false,
            "files": { name: { "content": content } },
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("gist API {}", response.status());
    }
    let body: serde_json::Value = response.json().await?;
    body.get("html_url")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("gist response has no html_url"))
}

/// Self-contained HTML export of the session (TS pi's /export default).
pub fn export_html(session: &SessionManager, target: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let messages = session.build_session_context().messages;
    let mut body = String::new();
    for message in &messages {
        match message {
            tack_agent_core::AgentMessage::User(u) => {
                let text = match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    tack_ai::UserContent::Blocks(b) => b
                        .iter()
                        .map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
                            tack_ai::InputContentBlock::Image { mime_type, .. } => {
                                format!("[image: {mime_type}]")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                body.push_str(&format!(
                    "<div class=\"msg user\"><div class=\"role\">you</div><div class=\"bubble\">{}</div></div>\n",
                    md_to_html(&text)
                ));
            }
            tack_agent_core::AgentMessage::Assistant(a) => {
                for block in &a.content {
                    match block {
                        tack_ai::ContentBlock::Text { text, .. } => {
                            body.push_str(&format!(
                                "<div class=\"msg assistant\"><div class=\"role\">assistant</div><div class=\"bubble\">{}</div></div>\n",
                                md_to_html(text)
                            ));
                        }
                        tack_ai::ContentBlock::Thinking { thinking, .. }
                            if !thinking.trim().is_empty() =>
                        {
                            body.push_str(&format!(
                                "<details class=\"thinking\"><summary>thinking</summary><pre>{}</pre></details>\n",
                                html_escape(thinking)
                            ));
                        }
                        tack_ai::ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            body.push_str(&format!(
                                "<div class=\"tool\"><span class=\"tool-name\">{}</span> <code>{}</code></div>\n",
                                html_escape(name),
                                html_escape(&arguments.to_string())
                            ));
                        }
                        _ => {}
                    }
                }
                if a.stop_reason == tack_ai::StopReason::Error {
                    body.push_str(&format!(
                        "<div class=\"error\">✗ {}</div>\n",
                        html_escape(a.error_message.as_deref().unwrap_or("error"))
                    ));
                }
            }
            tack_agent_core::AgentMessage::ToolResult(t) => {
                let text = t
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    body.push_str(&format!(
                        "<details class=\"result\"><summary>{} {}</summary><pre>{}</pre></details>\n",
                        if t.is_error { "✗" } else { "✓" },
                        html_escape(&t.tool_name),
                        html_escape(&text)
                    ));
                }
            }
            tack_agent_core::AgentMessage::BashExecution(b) => {
                body.push_str(&format!(
                    "<div class=\"tool\"><span class=\"tool-name\">$</span> <code>{}</code></div><pre class=\"result-open\">{}</pre>\n",
                    html_escape(&b.command),
                    html_escape(&b.output)
                ));
            }
            _ => {}
        }
    }

    let title = session.session_id();
    let doc = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>tack session {title}</title>\n\
         <style>\n\
         body {{ background: #0d1117; color: #e6edf3; font: 15px/1.6 -apple-system, \"Segoe UI\", sans-serif; max-width: 880px; margin: 24px auto; padding: 0 16px; }}\n\
         .role {{ color: #8b949e; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; margin-top: 18px; }}\n\
         .bubble {{ white-space: pre-wrap; }}\n\
         .bubble pre {{ background: #161b22; padding: 10px 12px; border-radius: 6px; overflow-x: auto; }}\n\
         .bubble code {{ background: #161b22; padding: 1px 5px; border-radius: 4px; }}\n\
         .bubble pre code {{ background: none; padding: 0; }}\n\
         .user .bubble {{ background: #1f2937; border-radius: 8px; padding: 10px 14px; }}\n\
         .tool {{ color: #c9d1d9; background: #161b22; border-left: 3px solid #58a6ff; padding: 6px 10px; margin: 6px 0; font-family: ui-monospace, monospace; font-size: 13px; }}\n\
         .tool-name {{ color: #58a6ff; font-weight: 600; }}\n\
         details.result, details.thinking {{ margin: 4px 0 4px 12px; color: #8b949e; }}\n\
         details pre {{ background: #161b22; padding: 10px 12px; border-radius: 6px; overflow-x: auto; white-space: pre-wrap; }}\n\
         .error {{ color: #f85149; }}\n\
         .result-open {{ background: #161b22; padding: 10px 12px; border-radius: 0 6px 6px 6px; overflow-x: auto; white-space: pre-wrap; }}\n\
         h1,h2,h3 {{ color: #82aaff; }}\n\
         a {{ color: #58a6ff; }}\n\
         blockquote {{ border-left: 3px solid #30363d; margin-left: 0; padding-left: 12px; color: #8b949e; }}\n\
         table {{ border-collapse: collapse; }} td, th {{ border: 1px solid #30363d; padding: 4px 10px; }}\n\
         </style></head><body>\n{body}\n</body></html>\n"
    );
    std::fs::write(target, doc).with_context(|| format!("write {}", target.display()))
}

/// Markdown → HTML via pulldown-cmark (same options as the TUI renderer).
fn md_to_html(text: &str) -> String {
    let options = pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TASKLISTS;
    let normalized = tack_tui::components::markdown::normalize_list_interruptions(text);
    let parser = pulldown_cmark::Parser::new_ext(&normalized, options);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Per-segment char counts for the `/context` table (chars/4 estimates;
/// the provider-reported usage shown below the table is authoritative).
#[derive(Default)]
pub(crate) struct ContextSegmentChars {
    pub(crate) user: usize,
    pub(crate) assistant: usize,
    pub(crate) thinking: usize,
    pub(crate) tool_results: std::collections::BTreeMap<String, usize>,
}

/// Segment the session history by message kind. Assistant tool-call blocks
/// count name + serialized arguments — those are real context tokens (often
/// the bulk of assistant output, e.g. large file writes) and must not be
/// silently dropped from the estimate.
pub(crate) fn context_segment_chars(
    messages: &[tack_agent_core::AgentMessage],
) -> ContextSegmentChars {
    let block_chars = |blocks: &[tack_ai::InputContentBlock]| -> usize {
        blocks
            .iter()
            .map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => text.chars().count(),
                tack_ai::InputContentBlock::Image { data, .. } => data.len(),
            })
            .sum()
    };
    let mut segments = ContextSegmentChars::default();
    for message in messages {
        match message {
            tack_agent_core::AgentMessage::User(m) => match &m.content {
                tack_ai::UserContent::Text(t) => segments.user += t.chars().count(),
                tack_ai::UserContent::Blocks(blocks) => segments.user += block_chars(blocks),
            },
            tack_agent_core::AgentMessage::Assistant(a) => {
                for block in &a.content {
                    match block {
                        tack_ai::ContentBlock::Text { text, .. } => {
                            segments.assistant += text.chars().count()
                        }
                        tack_ai::ContentBlock::Thinking { thinking, .. } => {
                            segments.thinking += thinking.chars().count()
                        }
                        tack_ai::ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            segments.assistant +=
                                name.chars().count() + arguments.to_string().chars().count();
                        }
                        _ => {}
                    }
                }
            }
            tack_agent_core::AgentMessage::ToolResult(r) => {
                *segments
                    .tool_results
                    .entry(r.tool_name.clone())
                    .or_default() += block_chars(&r.content);
            }
            tack_agent_core::AgentMessage::Custom(c) => match &c.content {
                tack_ai::UserContent::Text(t) => segments.user += t.chars().count(),
                tack_ai::UserContent::Blocks(blocks) => segments.user += block_chars(blocks),
            },
            tack_agent_core::AgentMessage::BashExecution(m) => {
                segments.user += m.command.chars().count() + m.output.chars().count();
            }
            tack_agent_core::AgentMessage::BranchSummary(m) => {
                segments.user += m.summary.chars().count();
            }
            tack_agent_core::AgentMessage::CompactionSummary(m) => {
                segments.user += m.summary.chars().count();
            }
            // Transcript state (upstream #9548): the prompt is already
            // accounted via context_system_chars; like upstream
            // estimateTokens, system messages count as 0 here.
            tack_agent_core::AgentMessage::System(_) => {}
        }
    }
    segments
}
