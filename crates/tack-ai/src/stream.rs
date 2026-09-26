//! Streaming primitives. Mirrors `packages/ai/src/utils/event-stream.ts`:
//! an awaitable event queue plus a final-result future.

use tokio::sync::{mpsc, oneshot};

use crate::types::{AssistantMessage, StopReason};

/// Receiving end: unbounded for provider streams (producers push
/// synchronously), bounded for the agent loop (backpressure + bounded
/// memory when the consumer stalls).
#[derive(Debug)]
enum EventRx<T> {
    Unbounded(mpsc::UnboundedReceiver<T>),
    Bounded(mpsc::Receiver<T>),
}

/// Receiving half of an event stream. Await `next()` for events and
/// `result()` for the final value once the stream completes.
#[derive(Debug)]
pub struct EventStream<T, R> {
    rx: EventRx<T>,
    result: oneshot::Receiver<R>,
}

/// Sending half. Clone-free: the producer task owns it.
#[derive(Debug)]
pub struct EventSender<T, R> {
    tx: mpsc::UnboundedSender<T>,
    result: Option<oneshot::Sender<R>>,
}

/// Create a connected event stream pair.
pub fn event_stream<T, R>() -> (EventSender<T, R>, EventStream<T, R>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (result_tx, result_rx) = oneshot::channel();
    (
        EventSender {
            tx,
            result: Some(result_tx),
        },
        EventStream {
            rx: EventRx::Unbounded(rx),
            result: result_rx,
        },
    )
}

impl<T, R> EventSender<T, R> {
    /// Push an event. Returns false if the receiver was dropped (consumer
    /// gone — the producer should stop work).
    pub fn push(&self, event: T) -> bool {
        self.tx.send(event).is_ok()
    }

    /// Complete the stream with its final result. Dropping the sender
    /// without calling `end` resolves `result()` with `Err(RecvError)`.
    pub fn end(mut self, result: R) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T, R> EventStream<T, R> {
    /// Build a stream from raw channel halves (used by producers that need
    /// the sender split across tasks, e.g. the agent loop).
    pub fn from_parts(rx: mpsc::UnboundedReceiver<T>, result: oneshot::Receiver<R>) -> Self {
        EventStream {
            rx: EventRx::Unbounded(rx),
            result,
        }
    }

    /// Build a stream from a BOUNDED channel: the producer side
    /// (`mpsc::Sender`) applies backpressure once `capacity` events are
    /// queued. Used by the agent loop so a stalled consumer bounds memory
    /// instead of accumulating ever-larger partial-message clones.
    pub fn from_parts_bounded(rx: mpsc::Receiver<T>, result: oneshot::Receiver<R>) -> Self {
        EventStream {
            rx: EventRx::Bounded(rx),
            result,
        }
    }

    /// Await the next event, or `None` when the stream is finished.
    pub async fn next(&mut self) -> Option<T> {
        match &mut self.rx {
            EventRx::Unbounded(rx) => rx.recv().await,
            EventRx::Bounded(rx) => rx.recv().await,
        }
    }

    /// Await the final result. Panics if the producer dropped without
    /// calling `end` (a producer bug — pi's contract is that streams always
    /// terminate with done/error which carries the result).
    ///
    /// INVARIANT: every producer in this crate (`api::*` adapters via
    /// `fail!`/`finish`, `RetryingProvider`, the agent loop) must route all
    /// exit paths through `finish`/`end`. Regression coverage for the
    /// decorator's drop paths lives in `tests/retry_tests.rs`
    /// (`retrying_provider_*_without_terminal_event`).
    pub async fn result(self) -> R {
        self.result
            .await
            .expect("event stream ended without a final result")
    }
}

// ---------------------------------------------------------------------------
// Assistant message events (port of AssistantMessageEvent from types.ts)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: crate::types::ContentBlock,
        partial: AssistantMessage,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// Terminal events carry the final `AssistantMessage` (pi's `result()`
    /// contract: `done` carries the message, `error` carries the error message).
    pub fn final_message(&self) -> Option<AssistantMessage> {
        match self {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            AssistantMessageEvent::Error { error, .. } => Some(error.clone()),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        )
    }

    /// The accumulated partial message carried by non-terminal events.
    pub fn partial(&self) -> Option<&AssistantMessage> {
        match self {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
        }
    }
}

/// The stream type returned by providers: events plus a final message.
pub type AssistantMessageEventStream = EventStream<AssistantMessageEvent, AssistantMessage>;
pub type AssistantMessageEventSender = EventSender<AssistantMessageEvent, AssistantMessage>;

impl AssistantMessageEventSender {
    /// Push a terminal event and end the stream with its message, mirroring
    /// pi's `push(done/error); end()` sequence.
    pub fn finish(self, event: AssistantMessageEvent) {
        let message = event
            .final_message()
            .expect("finish() requires a terminal (done/error) event");
        let _ = self.push(event);
        self.end(message);
    }
}
