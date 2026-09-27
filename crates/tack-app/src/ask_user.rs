//! TUI wiring for the `ask_user` tool: a dialog-backed
//! [`tack_tools::ask_user::AskUserHandler`] plus the main-loop state that
//! walks a batch of questions one dialog at a time.
//!
//! Mirrors the MCP elicitation flow (mcp_elicitation.rs): the tool's
//! execute future sends an [`AppEvent::AskUser`](crate::tui::AppEvent) and
//! parks on a oneshot; the main loop opens a SelectDialog (multiple-choice,
//! with a trailing "Other…" escape that re-asks the same question as
//! free text) or an InputDialog (free-text question) and resolves the
//! oneshot when the walk finishes or the user presses Esc.

use tokio::sync::oneshot;

use tack_tools::ask_user::{AskUserHandler, AskUserQuestion, AskUserResponse, answers_from};

/// SelectDialog item value for the "Other (custom answer)" escape: never a
/// plausible option label, so collisions with real choices cannot happen.
pub const CUSTOM_ANSWER_VALUE: &str = "__ask_user_custom_answer__";

/// A question batch forwarded from the tool to the TUI main loop.
#[derive(Debug)]
pub struct AskUserQuery {
    pub questions: Vec<AskUserQuestion>,
    pub respond: oneshot::Sender<AskUserResponse>,
}

/// Dialog-backed handler installed into `ToolServices` for TUI runs.
pub struct TuiAskUserHandler {
    tx: crate::tui::AppEventTx,
}

impl std::fmt::Debug for TuiAskUserHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiAskUserHandler").finish()
    }
}

impl TuiAskUserHandler {
    pub fn new(tx: crate::tui::AppEventTx) -> Self {
        TuiAskUserHandler { tx }
    }
}

#[async_trait::async_trait]
impl AskUserHandler for TuiAskUserHandler {
    async fn ask(&self, questions: Vec<AskUserQuestion>) -> AskUserResponse {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(crate::tui::AppEvent::AskUser(AskUserQuery {
                questions,
                respond,
            }))
            .is_err()
        {
            // TUI is shutting down: treat as dismissed.
            return AskUserResponse::Cancelled;
        }
        // Err(oneshot::Canceled) = dialog never answered (shutdown, or the
        // pending state was dropped): same in-band outcome as Esc.
        rx.await.unwrap_or(AskUserResponse::Cancelled)
    }
}

/// Dialog-side state for one in-flight batch (owned by the TUI while it
/// walks the questions one dialog at a time).
#[derive(Debug)]
pub struct PendingAskUser {
    questions: Vec<AskUserQuestion>,
    /// Index of the question the open dialog asks about.
    index: usize,
    answers: Vec<String>,
    /// The user picked "Other…" for the current multiple-choice question:
    /// the open dialog is the free-text InputDialog for the SAME question.
    pub awaiting_custom: bool,
    respond: oneshot::Sender<AskUserResponse>,
}

impl PendingAskUser {
    pub fn new(query: AskUserQuery) -> Self {
        PendingAskUser {
            questions: query.questions,
            index: 0,
            answers: Vec::new(),
            awaiting_custom: false,
            respond: query.respond,
        }
    }

    /// The question the current dialog is asking about.
    pub fn current(&self) -> Option<&AskUserQuestion> {
        self.questions.get(self.index)
    }

    /// (step, total) for dialog titles; step is 1-based.
    pub fn progress(&self) -> (usize, usize) {
        (self.index + 1, self.questions.len())
    }

    /// Record the current question's answer and advance.
    pub fn push_answer(&mut self, answer: String) {
        if self.current().is_some() {
            self.answers.push(answer);
            self.index += 1;
            self.awaiting_custom = false;
        }
    }

    /// All questions answered: resolve with the collected answers.
    pub fn finish(self) {
        let answers = answers_from(&self.questions, self.answers);
        let _ = self.respond.send(AskUserResponse::Answered(answers));
    }

    pub fn cancel(self) {
        let _ = self.respond.send(AskUserResponse::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_tools::ask_user::AskUserOption;

    fn questions() -> Vec<AskUserQuestion> {
        vec![
            AskUserQuestion {
                question: "which?".into(),
                header: None,
                options: Some(vec![
                    AskUserOption {
                        label: "A".into(),
                        description: None,
                    },
                    AskUserOption {
                        label: "B".into(),
                        description: None,
                    },
                ]),
            },
            AskUserQuestion {
                question: "why?".into(),
                header: None,
                options: None,
            },
        ]
    }

    #[tokio::test]
    async fn walk_collects_answers_in_order() {
        let (tx, rx) = crate::tui::app_event_bus();
        let handler = TuiAskUserHandler::new(tx);
        let call = tokio::spawn(async move { handler.ask(questions()).await });
        let Some(crate::tui::AppEvent::AskUser(query)) = rx.recv().await else {
            panic!("expected ask_user query");
        };
        assert_eq!(query.questions.len(), 2);

        let mut pending = PendingAskUser::new(query);
        assert_eq!(pending.progress(), (1, 2));
        assert_eq!(pending.current().unwrap().question, "which?");
        // "Other…" keeps the walk on the same question until text arrives.
        pending.awaiting_custom = true;
        pending.push_answer("custom".to_string());
        assert!(!pending.awaiting_custom);
        assert_eq!(pending.progress(), (2, 2));
        pending.push_answer("because".to_string());
        assert!(pending.current().is_none());
        pending.finish();

        let AskUserResponse::Answered(answers) = call.await.unwrap() else {
            panic!("expected answered");
        };
        assert_eq!(answers[0].answer, "custom");
        assert_eq!(answers[1].question, "why?");
        assert_eq!(answers[1].answer, "because");
    }

    #[tokio::test]
    async fn cancel_resolves_as_cancelled() {
        let (tx, rx) = crate::tui::app_event_bus();
        let handler = TuiAskUserHandler::new(tx);
        let call = tokio::spawn(async move { handler.ask(questions()).await });
        let Some(crate::tui::AppEvent::AskUser(query)) = rx.recv().await else {
            panic!("expected ask_user query");
        };
        PendingAskUser::new(query).cancel();
        assert_eq!(call.await.unwrap(), AskUserResponse::Cancelled);
    }

    #[tokio::test]
    async fn dropped_dialog_resolves_as_cancelled() {
        let (tx, rx) = crate::tui::app_event_bus();
        let handler = TuiAskUserHandler::new(tx);
        let call = tokio::spawn(async move { handler.ask(questions()).await });
        let Some(crate::tui::AppEvent::AskUser(query)) = rx.recv().await else {
            panic!("expected ask_user query");
        };
        // The pending state is dropped without answering (UI teardown).
        drop(PendingAskUser::new(query));
        assert_eq!(call.await.unwrap(), AskUserResponse::Cancelled);
    }
}
