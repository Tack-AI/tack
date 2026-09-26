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
        use tack_ext::protocol;
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
        if matches!(method.as_str(), "ui.select" | "ui.confirm" | "ui.input")
            && (self.dialog.is_some() || self.pending_ext_ui.is_some())
        {
            let _ = respond.send(Err("ui busy: a dialog is already open".to_string()));
            return;
        }
        match method.as_str() {
            "ui.notify" => {
                let parsed: protocol::NotifyParams =
                    serde_json::from_value(params).unwrap_or(protocol::NotifyParams {
                        message: "(bad notify params)".to_string(),
                        level: "warning".to_string(),
                    });
                let kind = match parsed.level.as_str() {
                    "warning" => NoticeKind::Warning,
                    "error" => NoticeKind::Error,
                    _ => NoticeKind::Info,
                };
                self.notice(parsed.message, kind);
                let _ = respond.send(Ok(serde_json::Value::Null));
            }
            "ui.set_status" => {
                let parsed: Option<protocol::SetStatusParams> = serde_json::from_value(params).ok();
                self.ext_label = parsed.and_then(|p| p.text);
                let _ = respond.send(Ok(serde_json::Value::Null));
            }
            "ui.select" => {
                let parsed: Result<protocol::SelectParams, _> = serde_json::from_value(params);
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
            "ui.confirm" => {
                let parsed: Result<protocol::ConfirmParams, _> = serde_json::from_value(params);
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
            "ui.input" => {
                let parsed: Result<protocol::InputParams, _> = serde_json::from_value(params);
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
            "exec" => {
                let parsed: Result<protocol::ExecParams, _> = serde_json::from_value(params);
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
            "provider.register" => {
                let parsed: Result<tack_ai::providers::RuntimeProviderSpec, _> =
                    serde_json::from_value(params);
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
                        let _ = respond.send(Err(format!("bad provider.register params: {e}")));
                    }
                }
            }
            method if method.starts_with("session.") => {
                let result = self.handle_ext_session_method(method, params).await;
                let _ = respond.send(result);
            }
            other => {
                let _ = respond.send(Err(format!("unknown method {other}")));
            }
        }
    }

    /// Plugin session-control methods (tack-ext `session.*`), executed on the
    /// TUI main loop.
    async fn handle_ext_session_method(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match method {
            "session.get_info" => {
                let context = self.state.session.build_session_context();
                Ok(serde_json::json!({
                    "sessionId": self.state.session.session_id(),
                    "cwd": self.cwd.to_string_lossy(),
                    "provider": self.state.model.provider,
                    "modelId": self.state.model.id,
                    "thinking": self.state.thinking.map(|t| t.as_str()).unwrap_or("off"),
                    "messageCount": context.messages.len(),
                    "running": self.running,
                }))
            }
            "session.new" => {
                self.run_command("new").await;
                Ok(serde_json::Value::Null)
            }
            "session.switch" => {
                let arg = params
                    .get("session")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.switch needs {session: path|id}")?;
                let dir = tack_session::default_session_dir(&self.cwd, &self.agent_dir);
                let path = tack_session::resolve_session_arg(arg, &dir)
                    .ok_or_else(|| format!("no session matching {arg:?}"))?;
                let session = SessionManager::open(&path, Some(dir)).map_err(|e| e.to_string())?;
                self.state.session = session;
                self.replay_transcript();
                Ok(serde_json::Value::Null)
            }
            "session.branch" => {
                let entry_id = params
                    .get("entryId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.branch needs {entryId}")?;
                self.summarize_abandoned_branch(entry_id).await;
                self.state
                    .session
                    .branch(entry_id)
                    .map_err(|e| e.to_string())?;
                self.replay_transcript();
                Ok(serde_json::Value::Null)
            }
            "session.set_model" => {
                let provider = params
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.set_model needs {provider, modelId}")?;
                let model_id = params
                    .get("modelId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.set_model needs {provider, modelId}")?;
                self.apply_select(
                    commands::SelectPurpose::Model,
                    &format!("{provider}/{model_id}"),
                )
                .await;
                Ok(serde_json::Value::Null)
            }
            "session.set_thinking" => {
                let level = params
                    .get("level")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.set_thinking needs {level}")?;
                self.apply_thinking(level).await;
                Ok(serde_json::Value::Null)
            }
            "session.set_name" => {
                let name = params
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.set_name needs {name}")?;
                self.state
                    .session
                    .append_session_info(Some(name.to_string()))
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::Value::Null)
            }
            "session.send_user_message" => {
                let text = params
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("session.send_user_message needs {text}")?;
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
