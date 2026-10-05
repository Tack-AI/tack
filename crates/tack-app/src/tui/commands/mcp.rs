//! MCP server toggle command (`/mcp`).

use tack_tui::components::select_list::SelectItem;

use super::super::{NoticeKind, TuiApp};
use super::{Dialog, SelectDialog, SelectPurpose};

impl TuiApp {
    /// `/mcp`: browse MCP server resources and prompts; picking one inserts
    /// its content into the editor (resources are read, prompts are fetched
    /// with empty arguments).
    pub(crate) async fn command_mcp(&mut self) {
        if self.mcp_connections.is_empty() {
            let mut specs = crate::mcp_config::configured_servers(&self.cwd, &self.agent_dir);
            specs.extend(self.extensions.bundle_mcp_servers.clone());
            if specs.is_empty() {
                self.notice(
                    crate::i18n::t(self.lang, "mcp.none_configured", &[]),
                    NoticeKind::Info,
                );
                return;
            }
            self.notice(
                crate::i18n::t(self.lang, "mcp.connecting", &[]),
                NoticeKind::Info,
            );
            self.mcp_connections = tack_tools::mcp::connect_all(specs).await;
            if self.mcp_connections.is_empty() {
                self.notice(
                    crate::i18n::t(self.lang, "mcp.none_connected", &[]),
                    NoticeKind::Warning,
                );
                return;
            }
        }
        let mut items = Vec::new();
        for (index, conn) in self.mcp_connections.iter().enumerate() {
            for resource in &conn.resources() {
                items.push(
                    SelectItem::new(
                        format!("📄 {}: {}", conn.name, resource.name),
                        format!("r\t{index}\t{}", resource.uri),
                    )
                    .with_description(resource.description.clone().unwrap_or_default()),
                );
            }
            for prompt in &conn.prompts() {
                items.push(
                    SelectItem::new(
                        format!("💬 {}: {}", conn.name, prompt.name),
                        format!("p\t{index}\t{}", prompt.name),
                    )
                    .with_description(prompt.description.clone().unwrap_or_default()),
                );
            }
        }
        if items.is_empty() {
            self.notice(
                crate::i18n::t(self.lang, "mcp.empty", &[]),
                NoticeKind::Info,
            );
            return;
        }
        self.dialog = Some(Dialog::Select(SelectDialog::new(
            crate::i18n::t(self.lang, "mcp.title", &[]),
            items,
            SelectPurpose::McpPick,
            self.theme,
        )));
    }

    pub(crate) async fn apply_mcp_pick(&mut self, value: &str) {
        let mut parts = value.splitn(3, '\t');
        let (Some(kind), Some(index), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
            return;
        };
        let Some(index) = index.parse::<usize>().ok() else {
            return;
        };
        let Some(conn) = self.mcp_connections.get(index).cloned() else {
            return;
        };
        match kind {
            "r" => {
                let request = rmcp::model::ReadResourceRequestParams::new(id);
                match conn.peer().read_resource(request).await {
                    Ok(result) => {
                        let text = result
                            .contents
                            .iter()
                            .map(tack_tools::mcp::resource_text)
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.insert_into_editor(&text);
                        self.notice(
                            crate::i18n::t(self.lang, "mcp.resource_inserted", &[("id", id)]),
                            NoticeKind::Info,
                        );
                    }
                    Err(e) => self.notice(
                        crate::i18n::t(self.lang, "mcp.read_failed", &[("error", &e.to_string())]),
                        NoticeKind::Error,
                    ),
                }
            }
            "p" => {
                let request = rmcp::model::GetPromptRequestParams::new(id);
                match conn.peer().get_prompt(request).await {
                    Ok(result) => {
                        let mut text = String::new();
                        for message in &result.messages {
                            let role = match message.role {
                                rmcp::model::Role::User => "user",
                                rmcp::model::Role::Assistant => "assistant",
                            };
                            let body = if let Some(t) = message.content.as_text() {
                                t.text.clone()
                            } else if message.content.as_image().is_some() {
                                crate::i18n::t(self.lang, "mcp.image", &[])
                            } else if let Some(resource) = message.content.as_resource() {
                                tack_tools::mcp::resource_text(&resource.resource)
                            } else {
                                crate::i18n::t(self.lang, "mcp.content", &[])
                            };
                            text.push_str(&format!("[{role}] {body}\n"));
                        }
                        self.insert_into_editor(text.trim_end());
                        self.notice(
                            crate::i18n::t(self.lang, "mcp.prompt_inserted", &[("id", id)]),
                            NoticeKind::Info,
                        );
                    }
                    Err(e) => self.notice(
                        crate::i18n::t(
                            self.lang,
                            "mcp.prompt_failed",
                            &[("error", &e.to_string())],
                        ),
                        NoticeKind::Error,
                    ),
                }
            }
            _ => {}
        }
    }
}
