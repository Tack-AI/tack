//! Bounded retry for assistant-producing calls. Port of
//! `packages/ai/src/utils/retry.ts` (retryAssistantCall + the transient-error
//! classifier) and a `RetryingProvider` decorator that applies the policy
//! transparently at the stream level.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::provider::{Provider, StreamOptions};
use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream, event_stream};
use crate::types::{AssistantMessage, Context, Model, StopReason};

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (the initial call never counts).
    pub max_retries: u32,
    /// Per-attempt delay is `base_delay_ms * 2^(attempt-1)`, capped by
    /// `max_agent_delay_ms`.
    pub base_delay_ms: u64,
    /// Cap for each computed retry delay (TS `maxAgentDelayMs`); `None`
    /// falls back to [`DEFAULT_MAX_AGENT_RETRY_DELAY_MS`].
    pub max_agent_delay_ms: Option<u64>,
}

/// TS `DEFAULT_MAX_AGENT_RETRY_DELAY_MS` (#8826): uncapped exponential
/// backoff made long retry streaks stall a run for minutes.
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

/// TS `retryDelayMs`: exponential delay for `attempt` (1-indexed), saturated
/// and capped at `max_agent_delay_ms` (default 60s).
pub fn retry_delay_ms(policy: &RetryPolicy, attempt: u32) -> u64 {
    let delay = policy
        .base_delay_ms
        .saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    delay.min(
        policy
            .max_agent_delay_ms
            .unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS),
    )
}

impl Default for RetryPolicy {
    /// Matches TS settings.retry defaults.
    fn default() -> Self {
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_agent_delay_ms: None,
        }
    }
}

const NON_RETRYABLE_PATTERNS: &[&str] = &[
    "gousagalimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
];

const RETRYABLE_PATTERNS: &[&str] = &[
    "overloaded",
    // Azure peak-load capacity errors (TS #9669).
    "currently experiencing high demand",
    "ratelimit",
    "rate limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    // Cloudflare origin errors (TS #9627).
    "520",
    "524",
    "service unavailable",
    "serviceunavailable",
    "server error",
    "servererror",
    "internal error",
    "internalerror",
    "provider returned error",
    "exceeded request buffer limit while retrying upstream",
    "network error",
    "networkerror",
    "connection error",
    "connection refused",
    "connection lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "enotfound",
    "eai_again",
    "upstream connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed out",
    "timedout",
    "timeout",
    "terminated",
    "websocket closed",
    "websocket error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "resourceexhausted",
    "transport closed",
    // Mid-stream transport failures: reqwest reports any response-body read
    // failure (connection reset, incomplete body, HTTP/2 stream errors, proxy
    // hiccups) as `error decoding response body`, which eventsource-stream
    // surfaces as `Transport error: ...`. These are transient and must be
    // retried, otherwise a single network hiccup kills the whole run.
    "error decoding response body",
    "error reading a body from connection",
    "transport error",
    "connection closed",
    "connection reset",
    "broken pipe",
    "unexpected end of file",
    "incomplete message",
    "channel closed",
];

/// Substring search with digit-word boundaries: an all-digit pattern (an
/// HTTP status like "500") must not match inside a longer digit run —
/// "15000 tokens" is not a 500 error. The char before and after the match
/// must not be ASCII digits.
fn contains_bounded(haystack: &str, needle: &str) -> bool {
    let numeric = needle.chars().all(|c| c.is_ascii_digit());
    if !numeric {
        return haystack.contains(needle);
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_digit());
        let after_ok = haystack[abs + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit());
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn contains_any(haystack: &str, patterns: &[&str]) -> bool {
    let lower = haystack.to_lowercase();
    let compact: String = lower
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .collect();
    patterns.iter().any(|p| {
        let pat = p.replace([' ', '-', '_'], "");
        contains_bounded(&lower, p) || contains_bounded(&compact, &pat)
    })
}

/// Classify whether a failed assistant message looks like a transient
/// provider/transport error worth retrying.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error) = &message.error_message else {
        return false;
    };
    if contains_any(error, NON_RETRYABLE_PATTERNS) {
        return false;
    }
    contains_any(error, RETRYABLE_PATTERNS)
}

/// (attempt, max_attempts, delay_ms, error_message) before each backoff.
pub type RetryScheduledCallback = Arc<dyn Fn(u32, u32, u64, String) + Send + Sync>;
/// (success, attempt, final_error) once when the loop ends.
pub type RetryFinishedCallback = Arc<dyn Fn(bool, u32, Option<String>) + Send + Sync>;

/// Callbacks around retries (pi's RetryCallbacks; all optional via Option).
#[derive(Clone, Default)]
pub struct RetryCallbacks {
    pub on_retry_scheduled: Option<RetryScheduledCallback>,
    pub on_retry_attempt_start: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_retry_finished: Option<RetryFinishedCallback>,
}

impl std::fmt::Debug for RetryCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryCallbacks").finish_non_exhaustive()
    }
}

/// Run a single assistant-producing call with bounded retry on transient
/// errors (port of retryAssistantCall).
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: RetryPolicy,
    cancel: CancellationToken,
    callbacks: RetryCallbacks,
) -> AssistantMessage
where
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = AssistantMessage> + Send,
{
    let max_attempts = if policy.enabled {
        policy.max_retries
    } else {
        0
    };
    let mut attempt = 0u32;
    let mut last_retry: Option<(u32, String)> = None;

    loop {
        let response = produce().await;

        if response.stop_reason == StopReason::Aborted {
            if let Some((attempt, _)) = last_retry
                && let Some(cb) = &callbacks.on_retry_finished
            {
                cb(false, attempt, None);
            }
            return response;
        }
        if response.stop_reason != StopReason::Error {
            if let Some((attempt, _)) = last_retry
                && let Some(cb) = &callbacks.on_retry_finished
            {
                cb(true, attempt, None);
            }
            return response;
        }
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((attempt, _)) = last_retry
                && let Some(cb) = &callbacks.on_retry_finished
            {
                cb(false, attempt, response.error_message.clone());
            }
            return response;
        }

        attempt += 1;
        let error_message = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".into());
        last_retry = Some((attempt, error_message.clone()));
        let delay_ms = retry_delay_ms(&policy, attempt);
        if let Some(cb) = &callbacks.on_retry_scheduled {
            cb(attempt, max_attempts, delay_ms, error_message.clone());
        }

        let abort_sleep = cancel.child_token();
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(cb) = &callbacks.on_retry_finished {
                    cb(false, attempt, Some(error_message));
                }
                let mut aborted = response;
                aborted.stop_reason = StopReason::Aborted;
                aborted.error_message = None;
                return aborted;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
        }
        drop(abort_sleep);
        if let Some(cb) = &callbacks.on_retry_attempt_start {
            cb();
        }
    }
}

/// A provider decorator that retries the whole stream when it terminates
/// with a retryable error. Partial events from failed attempts are forwarded
/// as-is (matching pi's streamFn-level retry), then a fresh attempt's events
/// follow.
pub struct RetryingProvider {
    pub inner: Arc<dyn Provider>,
    pub policy: RetryPolicy,
    /// Optional UI hook: (attempt, max_attempts, delay_ms, error) before each
    /// backoff sleep. Mirrors RetryCallbacks::on_retry_scheduled.
    pub on_retry_scheduled: Option<RetryScheduledCallback>,
}

impl std::fmt::Debug for RetryingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryingProvider").finish_non_exhaustive()
    }
}

impl Provider for RetryingProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let inner = self.inner.clone();
        let policy = self.policy;
        let on_retry_scheduled = self.on_retry_scheduled.clone();
        let model = model.clone();
        let context = context.clone();

        tokio::spawn(async move {
            let max_attempts = if policy.enabled {
                policy.max_retries
            } else {
                0
            };
            let mut attempt = 0u32;
            loop {
                let mut inner_stream = inner.stream(&model, &context, options.clone());
                let mut saw_terminal = false;
                while let Some(event) = inner_stream.next().await {
                    let final_message = event.final_message();
                    if final_message.is_some() {
                        saw_terminal = true;
                    }
                    if !event.is_terminal() {
                        if !sender.push(event) {
                            return; // consumer gone
                        }
                        continue;
                    }
                    let Some(message) = final_message else {
                        continue;
                    };
                    let retry = attempt < max_attempts
                        && is_retryable_assistant_error(&message)
                        && !options.cancel.is_cancelled();
                    if retry {
                        attempt += 1;
                        let delay_ms = retry_delay_ms(&policy, attempt);
                        let error = message
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "Unknown error".into());
                        tracing::warn!(
                            "provider stream failed ({error}); retry {attempt}/{max_attempts} in {delay_ms}ms"
                        );
                        if let Some(cb) = &on_retry_scheduled {
                            cb(attempt, max_attempts, delay_ms, error);
                        }
                        tokio::select! {
                            _ = options.cancel.cancelled() => {
                                let mut aborted = message;
                                aborted.stop_reason = StopReason::Aborted;
                                aborted.error_message = None;
                                sender.finish(AssistantMessageEvent::Error {
                                    reason: StopReason::Aborted,
                                    error: aborted,
                                });
                                return;
                            }
                            _ = async {
                                match &options.retry_cancel {
                                    Some(t) => t.cancelled().await,
                                    None => std::future::pending().await,
                                }
                            } => {
                                // Retry aborted (TS abortRetry): end the run
                                // with the last provider error instead of
                                // backing off — NOT a user abort.
                                tracing::info!("retry aborted by client");
                                sender.finish(AssistantMessageEvent::Error {
                                    reason: StopReason::Error,
                                    error: message,
                                });
                                return;
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                        }
                        break; // start a fresh attempt
                    }
                    // Terminal: forward and finish.
                    sender.finish(event);
                    return;
                }
                if !saw_terminal {
                    // Producer dropped without terminal event — treat as
                    // retryable transport failure if budget remains.
                    if attempt < max_attempts && !options.cancel.is_cancelled() {
                        attempt += 1;
                        // Same backoff contract as the error-event path
                        // above: exponential delay, on_retry_scheduled, and
                        // both cancellation scopes honored during the sleep.
                        let delay_ms = retry_delay_ms(&policy, attempt);
                        let error = "Provider stream ended without a terminal event".to_string();
                        tracing::warn!(
                            "provider stream dropped ({error}); retry {attempt}/{max_attempts} in {delay_ms}ms"
                        );
                        if let Some(cb) = &on_retry_scheduled {
                            cb(attempt, max_attempts, delay_ms, error.clone());
                        }
                        tokio::select! {
                            _ = options.cancel.cancelled() => {
                                let mut aborted = AssistantMessage::pending(&model);
                                aborted.stop_reason = StopReason::Aborted;
                                sender.finish(AssistantMessageEvent::Error {
                                    reason: StopReason::Aborted,
                                    error: aborted,
                                });
                                return;
                            }
                            _ = async {
                                match &options.retry_cancel {
                                    Some(t) => t.cancelled().await,
                                    None => std::future::pending().await,
                                }
                            } => {
                                // Retry aborted (TS abortRetry): end with the
                                // synthesized drop error, NOT a user abort.
                                tracing::info!("retry aborted by client");
                                let mut failed = AssistantMessage::pending(&model);
                                failed.stop_reason = StopReason::Error;
                                failed.error_message = Some(error);
                                sender.finish(AssistantMessageEvent::Error {
                                    reason: StopReason::Error,
                                    error: failed,
                                });
                                return;
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                        }
                        continue;
                    }
                    // Budget exhausted (or cancelled): the stream contract
                    // requires a terminal event carrying the final message —
                    // `EventStream::result()` panics without one — so
                    // synthesize an Error event instead of just returning.
                    let mut failed = AssistantMessage::pending(&model);
                    if options.cancel.is_cancelled() {
                        failed.stop_reason = StopReason::Aborted;
                        sender.finish(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: failed,
                        });
                    } else {
                        failed.stop_reason = StopReason::Error;
                        failed.error_message =
                            Some("Provider stream ended without a terminal event".to_string());
                        sender.finish(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: failed,
                        });
                    }
                    return;
                }
            }
        });

        stream
    }
}
