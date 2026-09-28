//! tack-ext extension protocol bridge (UI requests, session methods,
//! plugin exec). Inherent `impl TuiApp` split out of `mod.rs` — pure
//! code move, no behavior change.

use super::*;

impl TuiApp {
    /// Route a plugin's UI/exec request (tack-ext bridge).
    pub(crate) async fn handle_ext_ui_request(
        &mut self,
        request: crate::extension_host::ExtUiRequest,
    ) {
        use tack_ext::rpc3 as protocol;
        let crate::extension_host::ExtUiRequest {
            method,
            params,
            respond,
            ..
        } = request;
        // Dialog-producing requests never clobber an open dialog: replacing
        // a PERMISSION dialog drops its oneshot sender and silently denies
        // that tool call, and overwriting pending_ext_ui strands the earlier
        // plugin's responder. Decline with a busy error instead — the same
        // policy as the McpElicitation arm in mod.rs.
        if matches!(method.as_str(), "ui/select" | "ui/confirm" | "ui/input")
            && (self.dialog.is_some() || self.pending_ext_ui.is_some())
        {
            let _ = respond.send(Err("ui busy: a dialog is already open".to_string()));
            return;
        }
        match method.as_str() {
            "ui/notify" => {
                let parsed: protocol::UiNotifyParams =
                    serde_json::from_value(params).unwrap_or(protocol::UiNotifyParams {
                        message: "(bad notify params)".to_string(),
                        level: Some(protocol::LogLevel::Warning),
                    });
                let kind = match parsed.level {
                    Some(protocol::LogLevel::Warning) => NoticeKind::Warning,
                    Some(protocol::LogLevel::Error) => NoticeKind::Error,
                    _ => NoticeKind::Info,
                };
                self.notice(parsed.message, kind);
                let _ = respond.send(Ok(serde_json::Value::Null));
            }
            "ui/select" => {
                let parsed: Result<protocol::UiSelectParams, _> = serde_json::from_value(params);
                match parsed {
                    Ok(parsed) if !parsed.options.is_empty() => {
                        let items = parsed
                            .options
                            .iter()
                            .map(|o| {
                                tack_tui::components::select_list::SelectItem::new(
                                    o.clone(),
                                    o.clone(),
                                )
                            })
                            .collect();
                        self.pending_ext_ui = Some(("select".to_string(), respond));
                        self.dialog = Some(commands::Dialog::Select(commands::SelectDialog::new(
                            parsed.title,
                            items,
                            commands::SelectPurpose::ExtUi,
                            self.theme,
                        )));
                    }
                    _ => {
                        let _ = respond.send(Ok(serde_json::Value::Null));
                    }
                }
            }
            "ui/confirm" => {
                let parsed: Result<protocol::UiConfirmParams, _> = serde_json::from_value(params);
                match parsed {
                    Ok(parsed) => {
                        use tack_tui::components::select_list::SelectItem;
                        let items = vec![
                            SelectItem::new(crate::i18n::tr("permission.yes"), "yes"),
                            SelectItem::new(crate::i18n::tr("permission.no"), "no"),
                        ];
                        self.pending_ext_ui = Some(("confirm".to_string(), respond));
                        self.dialog = Some(commands::Dialog::Select(commands::SelectDialog::new(
                            format!("{} — {}", parsed.title, parsed.message),
                            items,
                            commands::SelectPurpose::ExtUi,
                            self.theme,
                        )));
                    }
                    _ => {
                        let _ = respond.send(Ok(serde_json::Value::Bool(false)));
                    }
                }
            }
            "ui/input" => {
                let parsed: Result<protocol::UiInputParams, _> = serde_json::from_value(params);
                match parsed {
                    Ok(parsed) => {
                        self.pending_ext_ui = Some(("input".to_string(), respond));
                        self.dialog = Some(commands::Dialog::Input(commands::InputDialog::new(
                            parsed.title,
                            parsed.placeholder.unwrap_or_default(),
                            self.theme,
                        )));
                    }
                    _ => {
                        let _ = respond.send(Ok(serde_json::Value::Null));
                    }
                }
            }
            "exec/run" => {
                let parsed: Result<protocol::ExecRunParams, _> = serde_json::from_value(params);
                match parsed {
                    Ok(parsed) => {
                        // Shell exec can run up to timeout_ms — never
                        // inline on the UI loop; the task responds.
                        tokio::spawn(async move {
                            let result = crate::ext_headless::run_ext_exec(&parsed).await;
                            let _ = respond
                                .send(serde_json::to_value(result).map_err(|e| e.to_string()));
                        });
                    }
                    _ => {
                        let _ = respond.send(Err("bad exec params".to_string()));
                    }
                }
            }
            "host/registerProvider" => {
                let provider = params
                    .get("provider")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let parsed: Result<tack_ai::providers::RuntimeProviderSpec, _> =
                    serde_json::from_value(provider);
                match parsed {
                    Ok(spec) => {
                        let id = spec.id.clone();
                        match tack_ai::providers::register_runtime_provider(spec) {
                            Ok(()) => {
                                self.notice(
                                    crate::i18n::t(
                                        self.lang,
                                        "msg.provider_registered",
                                        &[("id", &id)],
                                    ),
                                    NoticeKind::Info,
                                );
                                let _ = respond.send(Ok(serde_json::Value::Null));
                            }
                            Err(e) => {
                                let _ = respond.send(Err(e));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = respond.send(Err(format!("bad host/registerProvider params: {e}")));
                    }
                }
            }
            method if method.starts_with("session/") => {
                let result = self.handle_ext_session_method(method, params).await;
                let _ = respond.send(result);
            }
            other => {
                let _ = respond.send(Err(format!("unknown method {other}")));
            }
        }
    }

    /// Plugin session methods (tack-RPC v3 `session/*`), executed on the
    /// TUI main loop. The v3 surface is deliberately small: session/get +
    /// session/sendUserMessage.
    async fn handle_ext_session_method(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match method {
            "session/get" => {
                let context = self.state.session.build_session_context();
                Ok(serde_json::json!({
                    "sessionId": self.state.session.session_id(),
                    "mode": "tui",
                    "cwd": self.cwd.to_string_lossy(),
                    "trusted": true,
                    "provider": self.state.model.provider,
                    "modelId": self.state.model.id,
                    "thinking": self.state.thinking.map(|t| t.as_str()).unwrap_or("off"),
                    "messageCount": context.messages.len(),
                    "running": self.running,
                }))
            }
            "session/sendUserMessage" => {
                let text = params
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session/sendUserMessage needs {text}")?;
                if text.trim().is_empty() {
                    return Err("empty message".to_string());
                }
                if self.running {
                    self.follow_up.lock().await.push_back(text.to_string());
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Queued {
                            text: text.to_string(),
                            follow_up: true,
                        }));
                } else {
                    self.on_submit(text.to_string()).await;
                }
                Ok(serde_json::Value::Null)
            }
            other => Err(format!("unknown session method {other}")),
        }
    }
}
