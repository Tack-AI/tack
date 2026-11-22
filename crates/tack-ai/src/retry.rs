//! Bounded retry for assistant-producing calls. Port of
//! `packages/ai/src/utils/retry.ts` (retryAssistantCall + the transient-error
//! classifier) and a `RetryingProvider` decorator that applies the policy
//! transparently at the stream level.

use std::sync::{Arc, LazyLock};

use regex::Regex;
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
    // ChatGPT subscription and workspace limits reset after hours (or require
    // an administrator), rather than after a short transient throttle.
    "subscription_sharing_usage_limit_exceeded",
    "subscription_sharing_workplace_individual_usage_limit_exceeded",
];

/// Anthropic sends its temporary internal error as an HTTP 500 before a
/// stream, but as an SSE event without an HTTP status once streaming begins.
/// Match the exact JSON field so other Anthropic error types stay terminal.
static ANTHROPIC_API_ERROR_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"type"\s*:\s*"api_error""#).expect("Anthropic API error pattern compiles")
});

/// Z.ai's plan-level usage limit arrives as HTTP 429 with JSON code 1308.
/// The final non-digit guard keeps code 13080 from matching.
static ZAI_USAGE_LIMIT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"code"\s*:\s*"?1308(?:[^0-9]|$)"#).expect("z.ai usage limit pattern compiles")
});

const RETRYABLE_PATTERNS: &[&str] = &[
    "overloaded",
    // Azure peak-load capacity errors (TS #9669).
    "currently experiencing high demand",
    // Provider queue admission timeouts.
    "admission ceiling",
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
    // "temporarily" is required: a lasting provider routing/region error
    // must not be retried.
    "provider is temporarily unavailable",
    "provider temporarily unavailable",
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
    // Bare Node socket codes must be standalone tokens, not substrings of a
    // longer identifier such as EXECONNRESET or ERR_EPIPE_ID.
    "econnreset",
    "econnaborted",
    "econnrefused",
    "etimedout",
    "epipe",
    "connection reset by peer",
    "enotfound",
    "eai_again",
    "upstream connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed out",
    "timeout",
    "terminated",
    "websocket closed",
    "websocket error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "provider finish_reason: error",
    "returned an empty response",
    "stopped before completing the response",
    "ended the response before it was complete",
    "connection to the provider was reset",
    "reported a failure while generating the response",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "repeat the request later",
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

fn is_numeric_pattern(pattern: &str) -> bool {
    !pattern.is_empty() && pattern.chars().all(|c| c.is_ascii_digit())
}

fn is_socket_error_code(pattern: &str) -> bool {
    matches!(
        pattern,
        "econnreset" | "econnaborted" | "econnrefused" | "etimedout" | "epipe"
    )
}

/// Substring search with token boundaries for numeric HTTP statuses and
/// Node socket codes. A status must not be embedded in an ASCII alphanumeric
/// word (so a request id containing `503` is not an HTTP status), while a
/// socket code also treats `_` as a word character (`ERR_EPIPE_ID` is not
/// `EPIPE`). Other phrases retain the existing substring behavior.
fn contains_bounded(haystack: &str, needle: &str) -> bool {
    let numeric = is_numeric_pattern(needle);
    let socket_code = is_socket_error_code(needle);
    if !numeric && !socket_code {
        return haystack.contains(needle);
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let is_word_char = |c: char| c.is_ascii_alphanumeric() || (socket_code && c == '_');
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let after_ok = haystack[abs + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_word_char(c));
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
        if contains_bounded(&lower, p) {
            return true;
        }
        // Bounded tokens must occur verbatim in the original error. Removing
        // separators first could manufacture a status ("50_0") or socket code
        // ("E_PIPE") that was never present. Compacting "timed out" to
        // "timedout" would likewise make ETIMEDOUTX look transient after its
        // actual socket-code token guard rejects it.
        if is_numeric_pattern(p) || is_socket_error_code(p) || *p == "timed out" {
            return false;
        }
        let pat = p.replace([' ', '-', '_'], "");
        contains_bounded(&compact, &pat)
    })
}

/// Shared classifier for an initial HTTP failure body or an assistant error.
pub(crate) fn is_retryable_error_message(error: &str) -> bool {
    // Permanent account/plan limits always win over transient-looking HTTP
    // statuses or transport wording in the same error.
    if contains_any(error, NON_RETRYABLE_PATTERNS) || ZAI_USAGE_LIMIT_PATTERN.is_match(error) {
        return false;
    }
    ANTHROPIC_API_ERROR_PATTERN.is_match(error) || contains_any(error, RETRYABLE_PATTERNS)
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
    is_retryable_error_message(error)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, StopReason};

    fn error_message(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::pending(&Model {
            id: "m".into(),
            name: "m".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "http://x".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        });
        message.stop_reason = StopReason::Error;
        message.error_message = Some(text.into());
        message
    }

    #[test]
    fn retryable_table_covers_temporary_provider_and_socket_failures() {
        let cases = [
            "429 Too Many Requests",
            "waited 73.4s in queue, reaching this model's admission ceiling of 60s; retry after the hinted delay",
            "Provider finish_reason: error",
            "Provider finish_reason:error",
            "Provider returned an empty response",
            "Upstream error from Relace: The model stopped before completing the response.",
            "Unable to process request. Repeat the request later.",
            r#"{"type":"error","error":{"details":null,"type":"api_error","message":"Unable to complete this request right now. Please try again."}}"#,
            r#"{"type" : "api_error", "message":"temporary internal failure"}"#,
            "The provider is temporarily unavailable. Retry shortly.",
            "The upstream provider is temporarily unavailable. Try again shortly.",
            "The provider ended the response before it was complete.",
            "The connection to the provider was reset. Retry shortly.",
            "The provider reported a failure while generating the response.",
            "read ECONNRESET",
            "ECONNABORTED",
            "connect ECONNREFUSED 127.0.0.1:443",
            "connect ETIMEDOUT 140.82.113.3:443",
            "write EPIPE",
            "read (econnreset), retry",
            "Connection reset by peer",
        ];

        for error in cases {
            assert!(
                is_retryable_assistant_error(&error_message(error)),
                "expected retryable: {error}"
            );
        }
    }

    #[test]
    fn non_retryable_table_covers_limits_and_lookalike_identifiers() {
        let cases = [
            "The operation was aborted",
            "400 invalid request",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: at least one message is required"}}"#,
            r#"{"not_type":"api_error"}"#,
            r#"{"type":"api_errorish"}"#,
            "Provider finish_reason: content_filter",
            "Provider unavailable for this model",
            "The provider is unavailable in your region",
            "429 insufficient_quota: add credit",
            "503 quota exceeded for this month",
            "500 billing account is closed",
            "connection closed: available balance exhausted",
            "429 out of budget",
            r#"OpenAI API error (429): {"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached."}"#,
            r#"OpenAI API error (429): {"code":"subscription_sharing_workplace_individual_usage_limit_exceeded","message":"Usage limit reached."}"#,
            r#"429 {"code": 1308, "message":"Usage limit reached"}"#,
            r#"429 {"error":{"code":"1308","message":"Usage limit reached"}}"#,
            r#"{"type":"api_error","message":"Monthly quota exceeded"}"#,
            "Invalid value EXECONNRESET",
            "Unknown field ETIMEDOUTX",
            "Invalid key ERR_EPIPE_ID",
            "Invalid value E_PIPE",
            "Invalid key ECONNABORTED_",
            "Invalid key _ECONNREFUSED",
            "Invalid value ECONNRESET1",
        ];

        for error in cases {
            assert!(
                !is_retryable_assistant_error(&error_message(error)),
                "expected non-retryable: {error}"
            );
        }
    }

    #[test]
    fn http_status_table_requires_ascii_alphanumeric_boundaries() {
        let cases = [
            ("HTTP 500", true),
            ("upstream returned status=502.", true),
            ("503", true),
            ("429 ordinary transient throttle", true),
            (r#"429 {"code":13080,"message":"transient throttle"}"#, true),
            ("prompt too large: 15000 tokens exceeds limit", false),
            ("request id 4293123456 failed validation", false),
            (
                r#"401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011CU503xyz"}"#,
                false,
            ),
            ("request failed with code a503", false),
            ("request failed with code 503a", false),
            ("request failed with manufactured status 50_0", false),
            (
                "400 image exceeds 5 MB maximum: 5316784 bytes > 5242880 bytes",
                false,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(
                is_retryable_assistant_error(&error_message(error)),
                expected,
                "unexpected classification for: {error}"
            );
        }
    }

    #[test]
    fn classifier_requires_an_error_with_a_message() {
        let mut message = error_message("429 Too Many Requests");
        message.stop_reason = StopReason::Stop;
        assert!(!is_retryable_assistant_error(&message));

        message.stop_reason = StopReason::Error;
        message.error_message = None;
        assert!(!is_retryable_assistant_error(&message));
    }
}
