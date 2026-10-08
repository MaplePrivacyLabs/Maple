//! FIFO asynchronous events with a separately resolved final result.
//!
//! Port of `packages/ai/src/utils/event-stream.ts`. Provider operations use one
//! executor task, corresponding to Pi's independently progressing microtask.

use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::Stream;
use tokio::sync::oneshot;

use crate::types::{AssistantMessage, AssistantMessageEvent};

type Completion<T> = dyn Fn(&T) -> bool + Send + Sync;
type Extraction<T, R> = dyn Fn(&T) -> R + Send + Sync;

struct WaitingReader<T> {
    sender: oneshot::Sender<Option<T>>,
    closed: Arc<AtomicBool>,
}

struct State<T, R> {
    queue: VecDeque<T>,
    waiting: VecDeque<WaitingReader<T>>,
    done: bool,
    result: Option<R>,
    result_waiting: Vec<oneshot::Sender<R>>,
    is_complete: Arc<Completion<T>>,
    extract_result: Arc<Extraction<T, R>>,
}

impl<T, R: Clone> State<T, R> {
    fn resolve_result(&mut self, result: R) {
        // Like a JavaScript Promise, only the first resolution takes effect.
        if self.result.is_some() {
            return;
        }
        for waiting in self.result_waiting.drain(..) {
            let _ = waiting.send(result.clone());
        }
        self.result = Some(result);
    }
}

/// The sending half does not retain a producer, avoiding a reference cycle
/// when a provider's producer future captures its own event stream.
pub struct EventStreamWriter<T, R = T> {
    state: Arc<Mutex<State<T, R>>>,
}

impl<T, R> Clone for EventStreamWriter<T, R> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<T, R: Clone> EventStreamWriter<T, R> {
    pub fn push(&self, event: T) {
        let (is_complete, extract_result) = {
            let state = self.state.lock().expect("event stream state poisoned");
            if state.done {
                return;
            }
            (
                Arc::clone(&state.is_complete),
                Arc::clone(&state.extract_result),
            )
        };
        // Callbacks run outside the mutex: Pi permits synchronous reentry into
        // push/end/result. In particular, do not recheck done after is_complete.
        if is_complete(&event) {
            self.state.lock().expect("event stream state poisoned").done = true;
            let result = extract_result(&event);
            self.state
                .lock()
                .expect("event stream state poisoned")
                .resolve_result(result);
        }
        let mut state = self.state.lock().expect("event stream state poisoned");
        if let Some(waiter) = state.waiting.pop_front() {
            // A dropped receiver corresponds to an unobserved iterator promise;
            // it still consumes its place in the FIFO.
            let _ = waiter.sender.send(Some(event));
        } else {
            state.queue.push_back(event);
        }
    }

    pub fn end(&self, result: Option<R>) {
        let mut state = self.state.lock().expect("event stream state poisoned");
        state.done = true;
        if let Some(result) = result {
            state.resolve_result(result);
        }
        for waiter in state.waiting.drain(..) {
            waiter.closed.store(true, Ordering::Relaxed);
            let _ = waiter.sender.send(None);
        }
    }
}

/// Cloning creates another iterator over the same FIFO, not a broadcast
/// subscription. Waiting consumers receive events in registration order.
pub struct EventStream<T, R = T> {
    writer: EventStreamWriter<T, R>,
    next: Option<EventNext<'static, T>>,
    closed: Arc<AtomicBool>,
}

impl<T, R> Clone for EventStream<T, R> {
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            next: None,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<T, R: Clone> EventStream<T, R> {
    pub fn new(
        is_complete: impl Fn(&T) -> bool + Send + Sync + 'static,
        extract_result: impl Fn(&T) -> R + Send + Sync + 'static,
    ) -> Self {
        Self {
            writer: EventStreamWriter {
                state: Arc::new(Mutex::new(State {
                    queue: VecDeque::new(),
                    waiting: VecDeque::new(),
                    done: false,
                    result: None,
                    result_waiting: Vec::new(),
                    is_complete: Arc::new(is_complete),
                    extract_result: Arc::new(extract_result),
                })),
            },
            next: None,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn writer(&self) -> EventStreamWriter<T, R> {
        self.writer.clone()
    }

    pub fn push(&self, event: T) {
        self.writer.push(event);
    }

    pub fn end(&self, result: Option<R>) {
        self.writer.end(result);
    }

    /// Schedule one provider producer. Pi schedules this operation as a
    /// microtask independently of whether an iterator is observed. The caller
    /// performs synchronous submission effects before installing the producer.
    pub fn set_producer(&self, producer: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(producer);
    }

    /// Register a consumer immediately, matching `iterator.next()`'s Promise
    /// creation semantics even if the returned future has not yet been polled.
    /// The mutable borrow prevents overlapping next calls on one iterator;
    /// cloning creates another iterator with its own pending call.
    #[allow(clippy::should_implement_trait)] // Async, with eager registration unlike StreamExt::next.
    pub fn next(&mut self) -> EventNext<'_, T> {
        self.next.take().unwrap_or_else(|| self.register_next())
    }

    fn register_next(&self) -> EventNext<'static, T> {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .writer
            .state
            .lock()
            .expect("event stream state poisoned");
        if self.closed.load(Ordering::Relaxed) {
            let _ = sender.send(None);
        } else if let Some(event) = state.queue.pop_front() {
            let _ = sender.send(Some(event));
        } else if state.done {
            self.closed.store(true, Ordering::Relaxed);
            let _ = sender.send(None);
        } else {
            state.waiting.push_back(WaitingReader {
                sender,
                closed: self.closed.clone(),
            });
        }
        EventNext {
            receiver,
            iterator_borrow: PhantomData,
        }
    }

    pub fn result(&self) -> EventResult<R> {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .writer
            .state
            .lock()
            .expect("event stream state poisoned");
        if let Some(result) = &state.result {
            let _ = sender.send(result.clone());
        } else {
            state.result_waiting.push(sender);
        }
        EventResult {
            receiver: Some(receiver),
        }
    }
}

pub struct EventNext<'a, T> {
    receiver: oneshot::Receiver<Option<T>>,
    iterator_borrow: PhantomData<&'a mut ()>,
}

impl<T> Future for EventNext<'_, T> {
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver)
            .poll(cx)
            .map(|value| value.unwrap_or(None))
    }
}

pub struct EventResult<R> {
    receiver: Option<oneshot::Receiver<R>>,
}

impl<R> Future for EventResult<R> {
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(receiver) = self.receiver.as_mut() else {
            return Poll::Pending;
        };
        match Pin::new(receiver).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            // `end()` without a result never resolves the JS Promise, even if
            // its stream has gone away. Preserve that behavior here.
            Poll::Ready(Err(_)) => {
                self.receiver = None;
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, R> Unpin for EventStream<T, R> {}

impl<T, R: Clone> Stream for EventStream<T, R> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if self.next.is_none() {
            self.next = Some(self.register_next());
        }
        let result = Pin::new(self.next.as_mut().expect("iterator registered")).poll(cx);
        if result.is_ready() {
            self.next = None;
        }
        result
    }
}

pub type AssistantMessageEventStream = EventStream<AssistantMessageEvent, AssistantMessage>;
pub type AssistantMessageEventStreamWriter =
    EventStreamWriter<AssistantMessageEvent, AssistantMessage>;

pub fn create_assistant_message_event_stream() -> AssistantMessageEventStream {
    EventStream::new(
        |event| {
            matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            )
        },
        |event| match event {
            AssistantMessageEvent::Done { message, .. } => message.clone(),
            AssistantMessageEvent::Error { error, .. } => error.clone(),
            _ => unreachable!("Unexpected event type for final result"),
        },
    )
}
