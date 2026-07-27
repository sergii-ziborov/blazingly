#![forbid(unsafe_code)]

//! Runtime-neutral queue seam with an in-memory conformance adapter.
//!
//! This crate defines contracts only and stays dependency free. Vendor
//! adapters for NATS, `RabbitMQ`, Kafka, and SQS live in separate adapter
//! packages outside this repository and implement [`Queue`] there.
//! [`MemoryQueue`] exists for tests and local development, and [`Worker`]
//! supplies the retry, backoff, and dead-letter policy every adapter shares.
//!
//! The seam is deliberately cross-thread. [`QueueFuture`] carries a `Send`
//! bound and [`Queue`] requires `Send + Sync` so a [`Worker`] can run on a
//! multi-threaded pool next to the thread-per-core HTTP path. Operation
//! handlers stay unaffected: a `Send` future is usable from the framework's
//! thread-local executor, so publishing from a handler still compiles. An
//! adapter that can only run on one thread must own that thread internally and
//! hand back a `Send` future.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type QueueFuture<T> = Pin<Box<dyn Future<Output = Result<T, QueueError>> + Send + 'static>>;

/// Header naming the topic a dead-lettered delivery came from.
pub const DEAD_LETTER_SOURCE_HEADER: &str = "blazingly-dead-letter-source";
/// Header carrying the attempt count of a dead-lettered delivery.
pub const DEAD_LETTER_ATTEMPT_HEADER: &str = "blazingly-dead-letter-attempt";
/// Header carrying the last handler failure of a dead-lettered delivery.
pub const DEAD_LETTER_REASON_HEADER: &str = "blazingly-dead-letter-reason";

/// Queue payload shared by vendor adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub body: Vec<u8>,
    pub headers: BTreeMap<String, String>,
}

impl Message {
    #[must_use]
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            body: body.into(),
            headers: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }
}

/// One at-least-once delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delivery {
    pub receipt: String,
    pub attempt: u32,
    pub message: Message,
}

/// Adapter surface implemented by NATS, `RabbitMQ`, Kafka, SQS, and test
/// queues.
pub trait Queue: Clone + Send + Sync + 'static {
    /// Publishes one message to a topic.
    fn publish(&self, topic: &str, message: Message) -> QueueFuture<()>;

    /// Takes the next visible delivery from a topic, if any.
    fn receive(&self, topic: &str) -> QueueFuture<Option<Delivery>>;

    /// Settles a delivery so it is never redelivered.
    fn ack(&self, receipt: &str) -> QueueFuture<()>;

    /// Returns a delivery to its topic, invisible until `delay` has elapsed.
    ///
    /// The adapter raises the delivery's attempt count. It does not cap the
    /// attempts: the ceiling and the dead-letter route belong to [`Worker`].
    fn nack(&self, receipt: &str, delay: Duration) -> QueueFuture<()>;
}

/// Cloneable DI wrapper that keeps application code vendor-neutral.
#[derive(Clone)]
pub struct QueueClient<Adapter> {
    adapter: Adapter,
}

impl<Adapter> QueueClient<Adapter> {
    #[must_use]
    pub const fn new(adapter: Adapter) -> Self {
        Self { adapter }
    }

    #[must_use]
    pub const fn adapter(&self) -> &Adapter {
        &self.adapter
    }
}

impl<Adapter: Queue> QueueClient<Adapter> {
    pub fn publish(&self, topic: &str, message: Message) -> QueueFuture<()> {
        self.adapter.publish(topic, message)
    }

    pub fn receive(&self, topic: &str) -> QueueFuture<Option<Delivery>> {
        self.adapter.receive(topic)
    }

    pub fn ack(&self, receipt: &str) -> QueueFuture<()> {
        self.adapter.ack(receipt)
    }

    pub fn nack(&self, receipt: &str, delay: Duration) -> QueueFuture<()> {
        self.adapter.nack(receipt, delay)
    }
}

/// Deterministic in-memory adapter for tests and local development.
///
/// Visibility delays run on a logical clock advanced by [`MemoryQueue::advance`]
/// so retry behaviour is testable without a runtime timer.
#[derive(Clone, Default)]
pub struct MemoryQueue {
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    next_receipt: u64,
    now: Duration,
    topics: HashMap<String, VecDeque<PendingDelivery>>,
    in_flight: HashMap<String, (String, Delivery)>,
}

struct PendingDelivery {
    visible_at: Duration,
    delivery: Delivery,
}

impl MemoryQueue {
    /// Advances the logical clock so delayed deliveries become visible.
    pub fn advance(&self, elapsed: Duration) {
        let mut state = self.lock();
        state.now = state.now.saturating_add(elapsed);
    }

    /// Returns the deliveries waiting on a topic, visible or not.
    #[must_use]
    pub fn depth(&self, topic: &str) -> usize {
        self.lock().topics.get(topic).map_or(0, VecDeque::len)
    }

    /// Returns the deliveries currently checked out and unsettled.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Queue for MemoryQueue {
    fn publish(&self, topic: &str, message: Message) -> QueueFuture<()> {
        let topic = topic.to_owned();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.next_receipt = state.next_receipt.wrapping_add(1);
            let delivery = Delivery {
                receipt: format!("memory-{}", state.next_receipt),
                attempt: 1,
                message,
            };
            let visible_at = state.now;
            state
                .topics
                .entry(topic)
                .or_default()
                .push_back(PendingDelivery {
                    visible_at,
                    delivery,
                });
            Ok(())
        })
    }

    fn receive(&self, topic: &str) -> QueueFuture<Option<Delivery>> {
        let topic = topic.to_owned();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = state.now;
            let delivery = state.topics.get_mut(&topic).and_then(|pending| {
                let index = pending
                    .iter()
                    .position(|candidate| candidate.visible_at <= now)?;
                pending.remove(index).map(|entry| entry.delivery)
            });
            if let Some(delivery) = &delivery {
                state
                    .in_flight
                    .insert(delivery.receipt.clone(), (topic, delivery.clone()));
            }
            Ok(delivery)
        })
    }

    fn ack(&self, receipt: &str) -> QueueFuture<()> {
        let receipt = receipt.to_owned();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .in_flight
                .remove(&receipt)
                .ok_or(QueueError::UnknownReceipt(receipt))?;
            Ok(())
        })
    }

    fn nack(&self, receipt: &str, delay: Duration) -> QueueFuture<()> {
        let receipt = receipt.to_owned();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (topic, mut delivery) = state
                .in_flight
                .remove(&receipt)
                .ok_or(QueueError::UnknownReceipt(receipt))?;
            delivery.attempt = delivery.attempt.saturating_add(1);
            let visible_at = state.now.saturating_add(delay);
            state
                .topics
                .entry(topic)
                .or_default()
                .push_back(PendingDelivery {
                    visible_at,
                    delivery,
                });
            Ok(())
        })
    }
}

/// Failure returned by a worker job handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobError {
    message: String,
    retryable: bool,
}

impl JobError {
    /// Creates a failure the worker retries until the policy is exhausted.
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    /// Creates a failure the worker dead-letters without another attempt.
    #[must_use]
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    /// Returns the failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Reports whether the worker may attempt the delivery again.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

/// Bounded exponential backoff applied between delivery attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    multiplier: u32,
}

impl RetryPolicy {
    /// Creates a policy that stops after `max_attempts` deliveries.
    ///
    /// Values below one are clamped to one, so a delivery is always attempted.
    #[must_use]
    pub const fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: if max_attempts == 0 { 1 } else { max_attempts },
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            multiplier: 2,
        }
    }

    /// Sets the delay applied before the second attempt.
    #[must_use]
    pub const fn with_initial_backoff(mut self, initial_backoff: Duration) -> Self {
        self.initial_backoff = initial_backoff;
        self
    }

    /// Caps how long the backoff can grow.
    #[must_use]
    pub const fn with_max_backoff(mut self, max_backoff: Duration) -> Self {
        self.max_backoff = max_backoff;
        self
    }

    /// Sets the growth factor between attempts, clamped to at least one.
    #[must_use]
    pub const fn with_multiplier(mut self, multiplier: u32) -> Self {
        self.multiplier = if multiplier == 0 { 1 } else { multiplier };
        self
    }

    /// Returns the attempt ceiling.
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }

    /// Returns the delay applied before `attempt` is retried.
    #[must_use]
    pub fn backoff(self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(31);
        let factor = self.multiplier.checked_pow(exponent).unwrap_or(u32::MAX);
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }

    /// Reports whether `attempt` has reached the ceiling.
    #[must_use]
    pub const fn is_exhausted(self, attempt: u32) -> bool {
        attempt >= self.max_attempts
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(5)
    }
}

/// Destination for deliveries that exhausted their attempts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DeadLetter {
    /// Republish the message to another topic on the same queue.
    Topic(String),
    /// Acknowledge and drop the message.
    #[default]
    Discard,
}

/// Outcome of one [`Worker::step`] cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerStep {
    /// No delivery was visible on the topic.
    Idle,
    /// The handler succeeded and the delivery was acknowledged.
    Completed {
        /// Receipt of the settled delivery.
        receipt: String,
        /// Attempt number that succeeded.
        attempt: u32,
    },
    /// The handler failed and the delivery was scheduled for another attempt.
    Retried {
        /// Receipt of the returned delivery.
        receipt: String,
        /// Attempt number that failed.
        attempt: u32,
        /// Delay applied before the delivery becomes visible again.
        delay: Duration,
    },
    /// The delivery reached the dead-letter destination.
    DeadLettered {
        /// Receipt of the settled delivery.
        receipt: String,
        /// Attempt number that failed last.
        attempt: u32,
        /// Failure that ended the delivery.
        reason: JobError,
    },
}

/// Runtime-neutral consumer loop over one [`Queue`] topic.
///
/// The worker owns the retry ceiling, the exponential backoff, and the
/// dead-letter route; the adapter only has to honour the visibility delay
/// passed to [`Queue::nack`]. It never spawns a thread or reads a clock, so it
/// runs unchanged on any executor.
pub struct Worker<Adapter, Handler> {
    queue: Adapter,
    topic: String,
    handler: Handler,
    retry: RetryPolicy,
    dead_letter: DeadLetter,
    idle_backoff: Duration,
}

impl<Adapter, Handler> Worker<Adapter, Handler> {
    /// Creates a worker that consumes `topic` with `handler`.
    #[must_use]
    pub fn new(queue: Adapter, topic: impl Into<String>, handler: Handler) -> Self {
        Self {
            queue,
            topic: topic.into(),
            handler,
            retry: RetryPolicy::default(),
            dead_letter: DeadLetter::Discard,
            idle_backoff: Duration::from_millis(100),
        }
    }

    /// Replaces the retry policy.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Replaces the dead-letter destination.
    #[must_use]
    pub fn with_dead_letter(mut self, dead_letter: DeadLetter) -> Self {
        self.dead_letter = dead_letter;
        self
    }

    /// Sets how long [`Worker::run`] waits after an empty poll.
    #[must_use]
    pub const fn with_idle_backoff(mut self, idle_backoff: Duration) -> Self {
        self.idle_backoff = idle_backoff;
        self
    }

    /// Returns the consumed topic.
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }
}

impl<Adapter, Handler, HandlerFuture> Worker<Adapter, Handler>
where
    Adapter: Queue,
    Handler: Fn(Delivery) -> HandlerFuture + Send + Sync,
    HandlerFuture: Future<Output = Result<(), JobError>> + Send,
{
    /// Runs one receive, dispatch, and settle cycle.
    ///
    /// # Errors
    ///
    /// Returns the adapter failure raised by receive, ack, nack, or the
    /// dead-letter publish. A handler failure is not an error: it is settled by
    /// the retry policy and reported in the [`WorkerStep`].
    pub async fn step(&self) -> Result<WorkerStep, QueueError> {
        let Some(delivery) = self.queue.receive(&self.topic).await? else {
            return Ok(WorkerStep::Idle);
        };
        let receipt = delivery.receipt.clone();
        let attempt = delivery.attempt;
        let message = delivery.message.clone();
        let Err(reason) = (self.handler)(delivery).await else {
            self.queue.ack(&receipt).await?;
            return Ok(WorkerStep::Completed { receipt, attempt });
        };
        if reason.is_retryable() && !self.retry.is_exhausted(attempt) {
            let delay = self.retry.backoff(attempt);
            self.queue.nack(&receipt, delay).await?;
            return Ok(WorkerStep::Retried {
                receipt,
                attempt,
                delay,
            });
        }
        self.route_dead_letter(&receipt, attempt, message, &reason)
            .await?;
        Ok(WorkerStep::DeadLettered {
            receipt,
            attempt,
            reason,
        })
    }

    /// Runs cycles until the adapter fails, awaiting `sleep` on an empty poll.
    ///
    /// The caller owns the loop's lifetime and its timer: race this future
    /// against a shutdown signal from its own runtime.
    ///
    /// # Errors
    ///
    /// Returns the first adapter failure raised by a cycle.
    pub async fn run<Sleep, SleepFuture>(&self, sleep: Sleep) -> Result<(), QueueError>
    where
        Sleep: Fn(Duration) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        loop {
            if self.step().await? == WorkerStep::Idle {
                sleep(self.idle_backoff).await;
            }
        }
    }

    async fn route_dead_letter(
        &self,
        receipt: &str,
        attempt: u32,
        message: Message,
        reason: &JobError,
    ) -> Result<(), QueueError> {
        if let DeadLetter::Topic(destination) = &self.dead_letter {
            let message = message
                .with_header(DEAD_LETTER_SOURCE_HEADER, self.topic.clone())
                .with_header(DEAD_LETTER_ATTEMPT_HEADER, attempt.to_string())
                .with_header(DEAD_LETTER_REASON_HEADER, reason.message());
            self.queue.publish(destination, message).await?;
        }
        self.queue.ack(receipt).await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueError {
    Unavailable(String),
    UnknownReceipt(String),
}

impl fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "queue unavailable: {message}"),
            Self::UnknownReceipt(receipt) => write!(formatter, "unknown receipt `{receipt}`"),
        }
    }
}

impl std::error::Error for QueueError {}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future;
    use std::cell::Cell;

    fn worker_queue() -> MemoryQueue {
        let queue = MemoryQueue::default();
        future::block_on(queue.publish("jobs", Message::new("one"))).expect("publish");
        queue
    }

    #[test]
    fn memory_adapter_redelivers_nacked_messages_after_the_delay() {
        let queue = QueueClient::new(MemoryQueue::default());
        future::block_on(queue.publish("jobs", Message::new("one"))).expect("publish");
        let first = future::block_on(queue.receive("jobs"))
            .expect("receive")
            .expect("delivery");
        future::block_on(queue.nack(&first.receipt, Duration::from_secs(5))).expect("nack");
        assert!(
            future::block_on(queue.receive("jobs"))
                .expect("receive")
                .is_none()
        );
        queue.adapter().advance(Duration::from_secs(5));
        let second = future::block_on(queue.receive("jobs"))
            .expect("receive")
            .expect("redelivery");
        assert_eq!(second.attempt, 2);
        assert_eq!(second.message.body, b"one");
        future::block_on(queue.ack(&second.receipt)).expect("ack");
        assert_eq!(queue.adapter().in_flight(), 0);
    }

    #[test]
    fn unknown_receipts_are_rejected() {
        let queue = MemoryQueue::default();
        assert_eq!(
            future::block_on(queue.nack("missing", Duration::ZERO)),
            Err(QueueError::UnknownReceipt("missing".to_owned()))
        );
        assert_eq!(
            future::block_on(queue.ack("missing")),
            Err(QueueError::UnknownReceipt("missing".to_owned()))
        );
    }

    #[test]
    fn worker_acknowledges_successful_deliveries() {
        let queue = worker_queue();
        let worker = Worker::new(queue.clone(), "jobs", |_delivery| async { Ok(()) });
        let step = future::block_on(worker.step()).expect("step");
        assert!(matches!(step, WorkerStep::Completed { attempt: 1, .. }));
        assert_eq!(queue.depth("jobs"), 0);
        assert_eq!(queue.in_flight(), 0);
        assert_eq!(
            future::block_on(worker.step()).expect("step"),
            WorkerStep::Idle
        );
    }

    #[test]
    fn worker_retries_with_exponential_backoff_then_dead_letters() {
        let queue = worker_queue();
        let worker = Worker::new(queue.clone(), "jobs", |_delivery| async {
            Err(JobError::retryable("boom"))
        })
        .with_retry(
            RetryPolicy::new(3)
                .with_initial_backoff(Duration::from_secs(1))
                .with_max_backoff(Duration::from_secs(4)),
        )
        .with_dead_letter(DeadLetter::Topic("jobs.dead".to_owned()));

        let first = future::block_on(worker.step()).expect("first attempt");
        assert_eq!(
            first,
            WorkerStep::Retried {
                receipt: "memory-1".to_owned(),
                attempt: 1,
                delay: Duration::from_secs(1),
            }
        );
        assert_eq!(
            future::block_on(worker.step()).expect("step"),
            WorkerStep::Idle
        );

        queue.advance(Duration::from_secs(1));
        let second = future::block_on(worker.step()).expect("second attempt");
        assert_eq!(
            second,
            WorkerStep::Retried {
                receipt: "memory-1".to_owned(),
                attempt: 2,
                delay: Duration::from_secs(2),
            }
        );

        queue.advance(Duration::from_secs(2));
        let third = future::block_on(worker.step()).expect("third attempt");
        assert_eq!(
            third,
            WorkerStep::DeadLettered {
                receipt: "memory-1".to_owned(),
                attempt: 3,
                reason: JobError::retryable("boom"),
            }
        );
        assert_eq!(queue.depth("jobs"), 0);
        assert_eq!(queue.in_flight(), 0);

        let dead = future::block_on(queue.receive("jobs.dead"))
            .expect("receive")
            .expect("dead letter");
        assert_eq!(dead.message.body, b"one");
        assert_eq!(
            dead.message.headers.get(DEAD_LETTER_SOURCE_HEADER),
            Some(&"jobs".to_owned())
        );
        assert_eq!(
            dead.message.headers.get(DEAD_LETTER_ATTEMPT_HEADER),
            Some(&"3".to_owned())
        );
        assert_eq!(
            dead.message.headers.get(DEAD_LETTER_REASON_HEADER),
            Some(&"boom".to_owned())
        );
    }

    #[test]
    fn permanent_failures_skip_the_retry_budget() {
        let queue = worker_queue();
        let worker = Worker::new(queue.clone(), "jobs", |_delivery| async {
            Err(JobError::permanent("poison"))
        })
        .with_retry(RetryPolicy::new(10))
        .with_dead_letter(DeadLetter::Topic("jobs.dead".to_owned()));
        let step = future::block_on(worker.step()).expect("step");
        assert!(matches!(step, WorkerStep::DeadLettered { attempt: 1, .. }));
        assert_eq!(queue.depth("jobs"), 0);
        assert_eq!(queue.depth("jobs.dead"), 1);
    }

    #[test]
    fn discarded_dead_letters_are_acknowledged() {
        let queue = worker_queue();
        let worker = Worker::new(queue.clone(), "jobs", |_delivery| async {
            Err(JobError::permanent("poison"))
        })
        .with_retry(RetryPolicy::new(1));
        assert_eq!(worker.topic(), "jobs");
        future::block_on(worker.step()).expect("step");
        assert_eq!(queue.depth("jobs"), 0);
        assert_eq!(queue.in_flight(), 0);
    }

    #[derive(Clone, Default)]
    struct FlakyQueue {
        polls: Arc<Mutex<u32>>,
    }

    impl Queue for FlakyQueue {
        fn publish(&self, _topic: &str, _message: Message) -> QueueFuture<()> {
            Box::pin(async { Ok(()) })
        }

        fn receive(&self, _topic: &str) -> QueueFuture<Option<Delivery>> {
            let polls = Arc::clone(&self.polls);
            Box::pin(async move {
                let mut polls = polls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *polls += 1;
                if *polls > 2 {
                    return Err(QueueError::Unavailable("closed".to_owned()));
                }
                Ok(None)
            })
        }

        fn ack(&self, _receipt: &str) -> QueueFuture<()> {
            Box::pin(async { Ok(()) })
        }

        fn nack(&self, _receipt: &str, _delay: Duration) -> QueueFuture<()> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn run_idles_between_empty_polls_and_reports_adapter_failures() {
        let worker = Worker::new(FlakyQueue::default(), "jobs", |_delivery| async { Ok(()) })
            .with_idle_backoff(Duration::from_millis(5));
        let idles = Cell::new(0_u32);
        let error = future::block_on(worker.run(|delay| {
            assert_eq!(delay, Duration::from_millis(5));
            idles.set(idles.get() + 1);
            async {}
        }))
        .expect_err("adapter failure");
        assert_eq!(error, QueueError::Unavailable("closed".to_owned()));
        assert_eq!(idles.get(), 2);
    }

    #[test]
    fn backoff_grows_and_saturates() {
        let policy = RetryPolicy::new(4)
            .with_initial_backoff(Duration::from_millis(100))
            .with_max_backoff(Duration::from_secs(1))
            .with_multiplier(3);
        assert_eq!(policy.backoff(1), Duration::from_millis(100));
        assert_eq!(policy.backoff(2), Duration::from_millis(300));
        assert_eq!(policy.backoff(3), Duration::from_millis(900));
        assert_eq!(policy.backoff(4), Duration::from_secs(1));
        assert_eq!(policy.backoff(u32::MAX), Duration::from_secs(1));
        assert_eq!(policy.max_attempts(), 4);
        assert!(policy.is_exhausted(4));
        assert!(!policy.is_exhausted(3));
        assert_eq!(RetryPolicy::new(0).max_attempts(), 1);
        assert_eq!(
            RetryPolicy::new(2).with_multiplier(0).backoff(3),
            Duration::from_secs(1)
        );
    }
}
