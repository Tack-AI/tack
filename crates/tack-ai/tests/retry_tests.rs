//! Retry classifier + RetryingProvider tests.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tack_ai::retry::*;
use tack_ai::*;

fn error_message(text: &str) -> AssistantMessage {
    let mut m = AssistantMessage::pending(&Model {
        id: "m".into(),
        name: "m".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    });
    m.stop_reason = StopReason::Error;
    m.error_message = Some(text.into());
    m
}

#[test]
fn classifier_transient_vs_quota() {
    assert!(is_retryable_assistant_error(&error_message(
        "429 Too Many Requests"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "server overloaded, try later"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "socket hang up"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "Anthropic stream ended before message_stop"
    )));
    assert!(!is_retryable_assistant_error(&error_message(
        "insufficient_quota: add credit"
    )));
    assert!(!is_retryable_assistant_error(&error_message(
        "quota exceeded for this month"
    )));
    assert!(!is_retryable_assistant_error(&error_message(
        "invalid api key"
    )));
}

/// Regression test: mid-stream SSE transport failures (e.g. the server or an
/// intermediate proxy closing the connection of a long-lived stream) must be
/// classified as retryable. reqwest reports all response-body read failures
/// as "error decoding response body", surfaced through eventsource-stream as
/// "Transport error: ...". Previously none of the retryable patterns matched,
/// so a single network hiccup failed the whole run without any retry.
#[test]
fn classifier_midstream_transport_errors_are_retryable() {
    assert!(is_retryable_assistant_error(&error_message(
        "SSE stream error: Transport error: error decoding response body"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "SSE stream error: Transport error: error decoding response body: connection closed before message completed"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "SSE stream error: Transport error: error decoding response body: connection reset by peer"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "SSE stream error: Transport error: error decoding response body: unexpected end of file"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "error reading a body from connection: broken pipe"
    )));
    // Quota/billing errors that happen to mention connections stay non-retryable.
    assert!(!is_retryable_assistant_error(&error_message(
        "connection closed: available balance exhausted"
    )));
}

#[derive(Debug)]
struct FlakyProvider {
    calls: Arc<AtomicUsize>,
    fail_with: String,
    succeed_after: usize,
}

impl Provider for FlakyProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = n < self.succeed_after;
        let error = self.fail_with.clone();
        let model = model.clone();
        tokio::spawn(async move {
            let mut m = AssistantMessage::pending(&model);
            if fail {
                m.stop_reason = StopReason::Error;
                m.error_message = Some(error);
                sender.finish(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: m,
                });
            } else {
                m.stop_reason = StopReason::Stop;
                m.content = vec![ContentBlock::text("ok")];
                sender.finish(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: m,
                });
            }
        });
        stream
    }
}

fn fast_policy(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        enabled: true,
        max_retries,
        base_delay_ms: 1,
        max_agent_delay_ms: None,
    }
}

#[tokio::test]
async fn retrying_provider_recovers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(FlakyProvider {
        calls: calls.clone(),
        fail_with: "429 too many requests".into(),
        succeed_after: 2,
    });
    let provider = RetryingProvider {
        inner,
        policy: fast_policy(3),
        on_retry_scheduled: None,
    };
    let model = Model {
        id: "m".into(),
        name: "m".into(),
        api: "x".into(),
        provider: "x".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let ctx = Context::default();
    let message = provider
        .stream(&model, &ctx, StreamOptions::default())
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retrying_provider_recovers_from_midstream_transport_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(FlakyProvider {
        calls: calls.clone(),
        fail_with: "SSE stream error: Transport error: error decoding response body".into(),
        succeed_after: 1,
    });
    let provider = RetryingProvider {
        inner,
        policy: fast_policy(3),
        on_retry_scheduled: None,
    };
    let model = Model {
        id: "m".into(),
        name: "m".into(),
        api: "x".into(),
        provider: "x".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let ctx = Context::default();
    let message = provider
        .stream(&model, &ctx, StreamOptions::default())
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retrying_provider_gives_up_on_quota() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(FlakyProvider {
        calls: calls.clone(),
        fail_with: "insufficient_quota".into(),
        succeed_after: usize::MAX,
    });
    let provider = RetryingProvider {
        inner,
        policy: fast_policy(3),
        on_retry_scheduled: None,
    };
    let model = Model {
        id: "m".into(),
        name: "m".into(),
        api: "x".into(),
        provider: "x".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let ctx = Context::default();
    let message = provider
        .stream(&model, &ctx, StreamOptions::default())
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(calls.load(Ordering::SeqCst), 1); // no retries on quota errors
}

#[tokio::test]
async fn retrying_provider_exhausts_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(FlakyProvider {
        calls: calls.clone(),
        fail_with: "503 service unavailable".into(),
        succeed_after: usize::MAX,
    });
    let provider = RetryingProvider {
        inner,
        policy: fast_policy(2),
        on_retry_scheduled: None,
    };
    let model = Model {
        id: "m".into(),
        name: "m".into(),
        api: "x".into(),
        provider: "x".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let ctx = Context::default();
    let message = provider
        .stream(&model, &ctx, StreamOptions::default())
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(calls.load(Ordering::SeqCst), 3); // 1 initial + 2 retries
}

/// abortRetry semantics: cancelling the retry scope during a backoff ends the
/// stream with the LAST provider error (not Aborted) and makes no further
/// attempts — the run is not user-aborted.
#[tokio::test]
async fn retry_cancel_ends_backoff_with_last_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(FlakyProvider {
        calls: calls.clone(),
        fail_with: "429 too many requests".into(),
        succeed_after: usize::MAX, // every attempt fails
    });
    // Long backoff so the retry window is observable.
    let policy = RetryPolicy {
        enabled: true,
        max_retries: 5,
        base_delay_ms: 60_000,
        // Keep the long backoff observable: above the default 60s cap.
        max_agent_delay_ms: Some(300_000),
    };
    let provider = RetryingProvider {
        inner,
        policy,
        on_retry_scheduled: None,
    };
    let model = Model {
        id: "m".into(),
        name: "m".into(),
        api: "x".into(),
        provider: "x".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    };
    let ctx = Context::default();
    let retry_cancel = tokio_util::sync::CancellationToken::new();
    let options = StreamOptions {
        retry_cancel: Some(retry_cancel.clone()),
        ..Default::default()
    };
    let stream = provider.stream(&model, &ctx, options);

    // Cancel the retry backoff after the first failure is being slept off.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    retry_cancel.cancel();

    let message = stream.result().await;
    assert_eq!(
        message.stop_reason,
        StopReason::Error,
        "last error, not abort"
    );
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("429")
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no further attempts after abortRetry"
    );
}

/// Regression: a bare number inside a larger digit run (e.g. a token count)
/// must not be classified as an HTTP status ("15000 tokens" ≠ 500).
#[test]
fn classifier_digit_patterns_need_word_boundaries() {
    assert!(!is_retryable_assistant_error(&error_message(
        "prompt too large: 15000 tokens exceeds limit"
    )));
    assert!(!is_retryable_assistant_error(&error_message(
        "request id 4293123456 failed validation"
    )));
    // Real status codes (digit boundaries) still classify as retryable.
    assert!(is_retryable_assistant_error(&error_message("HTTP 500")));
    assert!(is_retryable_assistant_error(&error_message(
        "503 service unavailable"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "429 Too Many Requests"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "upstream returned status 502."
    )));
}

/// A provider whose stream task drops the sender without ever pushing a
/// terminal (done/error) event — a producer bug the decorator must survive.
#[derive(Debug)]
struct SilentProvider {
    calls: Arc<AtomicUsize>,
}

impl Provider for SilentProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (_sender, stream) = event_stream();
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = model;
        // Deliberately never finish: drop the sender at scope end.
        stream
    }
}

fn silent_model() -> Model {
    Model {
        id: "m".into(),
        name: "m".into(),
        api: "x".into(),
        provider: "x".into(),
        base_url: "http://x".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Regression: when the retry budget is exhausted on a producer that never
/// emits a terminal event, the decorator must synthesize a terminal Error
/// event — `stream.result()` must resolve, not panic.
#[tokio::test]
async fn retrying_provider_synthesizes_terminal_event_for_silent_producer() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(SilentProvider {
        calls: calls.clone(),
    });
    let provider = RetryingProvider {
        inner,
        policy: fast_policy(2),
        on_retry_scheduled: None,
    };
    let ctx = Context::default();
    let message = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider
            .stream(&silent_model(), &ctx, StreamOptions::default())
            .result(),
    )
    .await
    .expect("result() must resolve");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("terminal event")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3); // 1 initial + 2 retries
}

/// Same, but with the cancel token already cancelled: the synthesized
/// terminal event is an Abort, and no retries are spent.
#[tokio::test]
async fn retrying_provider_silent_producer_cancel_aborts() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(SilentProvider {
        calls: calls.clone(),
    });
    let provider = RetryingProvider {
        inner,
        policy: fast_policy(2),
        on_retry_scheduled: None,
    };
    let ctx = Context::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let options = StreamOptions {
        cancel,
        ..Default::default()
    };
    let message = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.stream(&silent_model(), &ctx, options).result(),
    )
    .await
    .expect("result() must resolve");
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(calls.load(Ordering::SeqCst), 1); // no retries when cancelled
}

/// Regression: a dropped (terminal-less) stream must back off like the
/// error-event path — on_retry_scheduled fires with an exponential delay
/// before each subsequent attempt.
#[tokio::test]
async fn retrying_provider_backs_off_after_dropped_stream() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(SilentProvider {
        calls: calls.clone(),
    });
    let scheduled: Arc<std::sync::Mutex<Vec<(u32, u64)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let scheduled_cb = scheduled.clone();
    let provider = RetryingProvider {
        inner,
        policy: RetryPolicy {
            enabled: true,
            max_retries: 2,
            base_delay_ms: 1,
            max_agent_delay_ms: None,
        },
        on_retry_scheduled: Some(Arc::new(move |attempt, _max, delay_ms, _error| {
            scheduled_cb.lock().unwrap().push((attempt, delay_ms));
        })),
    };
    let ctx = Context::default();
    let message = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider
            .stream(&silent_model(), &ctx, StreamOptions::default())
            .result(),
    )
    .await
    .expect("result() must resolve");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(calls.load(Ordering::SeqCst), 3); // 1 initial + 2 retries
    // Exponential schedule: base * 2^(attempt-1) = 1ms, 2ms.
    assert_eq!(
        *scheduled.lock().unwrap(),
        vec![(1, 1), (2, 2)],
        "each dropped-stream retry must be announced with its backoff delay"
    );
}

/// Regression: cancelling the retry scope while a dropped-stream retry is
/// backing off must end the stream with the drop error (not Aborted) and
/// make no further attempts — the previous code ignored retry_cancel and
/// spun immediately.
#[tokio::test]
async fn retry_cancel_ends_dropped_stream_backoff() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(SilentProvider {
        calls: calls.clone(),
    });
    // Long backoff so the retry window is observable.
    let provider = RetryingProvider {
        inner,
        policy: RetryPolicy {
            enabled: true,
            max_retries: 5,
            base_delay_ms: 60_000,
            // Keep the long backoff observable: above the default 60s cap.
            max_agent_delay_ms: Some(300_000),
        },
        on_retry_scheduled: None,
    };
    let ctx = Context::default();
    let retry_cancel = tokio_util::sync::CancellationToken::new();
    let options = StreamOptions {
        retry_cancel: Some(retry_cancel.clone()),
        ..Default::default()
    };
    let stream = provider.stream(&silent_model(), &ctx, options);

    // Cancel the retry backoff after the first drop is being slept off.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    retry_cancel.cancel();

    let message = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
        .await
        .expect("retry_cancel must end the backoff promptly");
    assert_eq!(
        message.stop_reason,
        StopReason::Error,
        "drop error, not abort"
    );
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("terminal event")
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no further attempts after abortRetry"
    );
}

/// TS #8826: the computed exponential delay is capped by `max_agent_delay_ms`
/// (falling back to the 60s default), and saturates instead of overflowing.
#[test]
fn retry_delay_is_capped() {
    let mut policy = fast_policy(3);
    policy.base_delay_ms = 10_000;
    assert_eq!(retry_delay_ms(&policy, 1), 10_000);
    assert_eq!(retry_delay_ms(&policy, 3), 40_000);
    assert_eq!(retry_delay_ms(&policy, 4), 60_000); // min(80s, default 60s)
    assert_eq!(retry_delay_ms(&policy, 30), 60_000); // no overflow
    policy.max_agent_delay_ms = Some(5_000);
    assert_eq!(retry_delay_ms(&policy, 1), 5_000);
    assert_eq!(retry_delay_ms(&policy, 30), 5_000);
}

/// TS #9627/#9669: Cloudflare 520 and Azure peak-load capacity errors are
/// classified as retryable.
#[test]
fn retryable_covers_cloudflare_520_and_azure_peak_load() {
    assert!(is_retryable_assistant_error(&error_message(
        "HTTP 520: web server returns an unknown error"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "The server is currently experiencing high demand. Please try again later."
    )));
}
