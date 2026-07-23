use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::{oneshot, watch},
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::actor::{
    binding::{
        ActorStats, ActorStatsCounters, BindingCore, BindingState, MailboxReceiver, MailboxRef,
        MessageSizeObserver, SendOutcome,
    },
    error::{CallError, SendError, TryRecvError},
    monitor::{ActorMonitors, MonitorEvent, MonitorHub, MonitorRef},
    observability::{GraphObservability, MessageOperation, SendRejection, trace_actor_message},
};

/// Cloneable, restart-stable, typed sender for an actor mailbox.
///
/// An `ActorRef<M>` is bound to a long-lived mailbox binding rather than a
/// single actor runtime instance. When the target actor is restarted (either
/// as part of a graph rerun or via per-actor supervision), the handle
/// transparently follows the new mailbox.
pub struct ActorRef<M> {
    actor_id: Arc<str>,
    binding: watch::Receiver<BindingState<M>>,
    stats: Arc<ActorStatsCounters>,
    message_size: Option<Arc<MessageSizeObserver<M>>>,
    source_actor_id: Option<Arc<str>>,
    monitors: Arc<MonitorHub>,
}

impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        Self {
            actor_id: Arc::clone(&self.actor_id),
            binding: self.binding.clone(),
            stats: Arc::clone(&self.stats),
            message_size: self.message_size.clone(),
            source_actor_id: self.source_actor_id.clone(),
            monitors: Arc::clone(&self.monitors),
        }
    }
}

impl<M> fmt::Debug for ActorRef<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActorRef")
            .field("actor_id", &self.actor_id)
            .finish_non_exhaustive()
    }
}

impl<M> ActorRef<M> {
    pub(crate) fn from_core(core: &Arc<BindingCore<M>>, source_actor_id: Option<Arc<str>>) -> Self {
        Self::from_parts(
            core.actor_id().clone(),
            core.subscribe(),
            core.stats_counters(),
            core.message_size(),
            source_actor_id,
            core.monitor_hub(),
        )
    }

    pub(crate) fn from_parts(
        actor_id: Arc<str>,
        binding: watch::Receiver<BindingState<M>>,
        stats: Arc<ActorStatsCounters>,
        message_size: Option<Arc<MessageSizeObserver<M>>>,
        source_actor_id: Option<Arc<str>>,
        monitors: Arc<MonitorHub>,
    ) -> Self {
        Self {
            actor_id,
            binding,
            stats,
            message_size,
            source_actor_id,
            monitors,
        }
    }

    pub(crate) fn detached(actor_id: Arc<str>) -> Self {
        let core = Arc::new(BindingCore::<M>::new(actor_id));
        Self::from_core(&core, None)
    }

    pub(crate) fn detached_with_size_hint(actor_id: Arc<str>, size_hint: fn(&M) -> usize) -> Self {
        let core = Arc::new(BindingCore::<M>::with_message_size(actor_id, size_hint));
        Self::from_core(&core, None)
    }

    /// Returns the target actor id.
    pub fn id(&self) -> &str {
        &self.actor_id
    }

    /// Returns a point-in-time snapshot of this actor's message counters and
    /// current mailbox usage.
    pub fn stats(&self) -> ActorStats {
        let (depth, capacity) = match &*self.binding.borrow() {
            BindingState::Bound(mailbox) => mailbox.usage(),
            BindingState::Unbound | BindingState::Terminated => (0, 0),
        };
        self.stats.snapshot(&self.actor_id, depth, capacity)
    }

    fn current_mailbox(&self) -> Result<MailboxRef<M>, SendError> {
        match self.binding.borrow().clone() {
            BindingState::Bound(mailbox) => Ok(mailbox),
            BindingState::Unbound if self.binding.has_changed().is_err() => {
                Err(self.actor_terminated())
            }
            BindingState::Unbound => Err(SendError::ActorNotRunning {
                actor_id: self.actor_id.to_string(),
            }),
            BindingState::Terminated => Err(self.actor_terminated()),
        }
    }

    /// Sends a message to the target actor.
    ///
    /// This waits until the actor has a bound mailbox, waits for capacity when
    /// the actor uses a FIFO queue, and rides through restart windows when the
    /// actor is expected to rebind. Conflating mailboxes replace stale unread
    /// state immediately instead of waiting for capacity. This returns an
    /// error only when the actor has terminated with no restart scheduled, or
    /// when the binding source has been dropped.
    ///
    /// Cancelling this future while it is waiting drops the message.
    ///
    /// # Delivery contract
    ///
    /// Delivery is **at-most-once**. `Ok` means the message was accepted by
    /// the current incarnation's mailbox, not that it will be processed:
    /// mailboxes are incarnation-owned, so messages accepted by an
    /// incarnation that stops before reading them are lost with it. The loss
    /// windows are restart and shutdown. Stronger guarantees
    /// (acknowledgements, redelivery) are user protocol built with
    /// [`call`](Self::call) and [`Reply`], not transport features.
    pub async fn send(&self, message: M) -> Result<(), SendError> {
        let mut binding = self.binding.clone();
        let mut message = message;
        let message_size = self
            .message_size
            .as_ref()
            .map(|observer| observer.size_hint(&message));

        loop {
            let mailbox = match self.wait_for_next_mailbox(&mut binding).await {
                Ok(mailbox) => mailbox,
                Err(error) => {
                    self.observe_send(MessageOperation::Send, Some(send_rejection(&error)));
                    self.stats.record_send(false);
                    return Err(error);
                }
            };

            match mailbox.send_retaining(message).await {
                SendOutcome::Accepted { conflated } => {
                    self.observe_send(MessageOperation::Send, None);
                    self.stats.record_send(true);
                    self.stats.record_conflated(conflated);
                    self.record_message_size(message_size);
                    return Ok(());
                }
                SendOutcome::Closed(returned) => {
                    self.observe_send(MessageOperation::Send, Some(SendRejection::MailboxClosed));
                    message = returned;
                    if let Err(error) = self
                        .wait_for_rebind_or_termination(&mut binding, &mailbox)
                        .await
                    {
                        self.observe_send(MessageOperation::Send, Some(send_rejection(&error)));
                        self.stats.record_send(false);
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Attempts to send a message without waiting for mailbox capacity.
    ///
    /// A full FIFO queue returns [`SendError::MailboxFull`]. A conflating
    /// mailbox instead accepts the message and replaces stale unread state.
    pub fn try_send(&self, message: M) -> Result<(), SendError> {
        let message_size = self
            .message_size
            .as_ref()
            .map(|observer| observer.size_hint(&message));
        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox.try_send(message),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::TrySend,
            result.as_ref().err().map(send_rejection),
        );
        self.stats.record_send(result.is_ok());
        match result {
            Ok(conflated) => {
                self.stats.record_conflated(conflated);
                self.record_message_size(message_size);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Sends a request and waits for the actor to answer through the
    /// [`Reply`] carried inside the message.
    ///
    /// Callers own their deadline. Compose `call` with
    /// [`tokio::time::timeout`] for a bounded wait:
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use tokio::time::timeout;
    /// use tokio_otp::{ActorRef, Reply};
    ///
    /// enum Msg {
    ///     Get(Reply<u64>),
    /// }
    ///
    /// # async fn get(actor: &ActorRef<Msg>) -> Result<u64, Box<dyn std::error::Error>> {
    /// let value = timeout(Duration::from_millis(250), actor.call(Msg::Get)).await??;
    /// # Ok(value)
    /// # }
    /// ```
    ///
    /// This waits for the same actor binding conditions as [`send`](Self::send),
    /// including FIFO mailbox capacity and expected restart windows. If the
    /// timeout cancels `call` before the request is accepted, the request is
    /// dropped. Once the mailbox accepts it, cancelling the caller's wait
    /// cannot retract it: the actor may still process the request and a late
    /// reply is discarded.
    ///
    /// Consequently, a timeout after acceptance has an **unknown outcome**.
    /// In-memory queries are usually harmless, but requests with external
    /// side effects need protocol-level idempotency keys and/or reconciliation
    /// so the caller can safely retry or discover what happened. A timeout is
    /// not an actor-work cancellation signal.
    ///
    /// Do not use `call` with a conflating mailbox: a newer message may replace
    /// the request before it is handled, in which case this returns
    /// [`CallError::ReplyDropped`]. Conflating mailboxes are for state
    /// snapshots rather than request/response commands.
    pub async fn call<T>(&self, message: impl FnOnce(Reply<T>) -> M) -> Result<T, CallError> {
        let (sender, receiver) = oneshot::channel();
        self.send(message(Reply { sender })).await?;
        receiver.await.map_err(|_| CallError::ReplyDropped {
            actor_id: self.actor_id.to_string(),
        })
    }

    async fn wait_for_next_mailbox(
        &self,
        binding: &mut watch::Receiver<BindingState<M>>,
    ) -> Result<MailboxRef<M>, SendError> {
        loop {
            match binding.borrow().clone() {
                BindingState::Bound(mailbox) => return Ok(mailbox),
                BindingState::Unbound => {}
                BindingState::Terminated => return Err(self.actor_terminated()),
            }

            binding
                .changed()
                .await
                .map_err(|_| self.actor_terminated())?;
        }
    }

    /// Waits until the stale mailbox is unbound and a fresh one is bound.
    ///
    /// Waiting for the stale mailbox to clear first avoids busy-looping in
    /// the window where an actor's mailbox is already closed but its binding
    /// has not been cleared yet.
    async fn wait_for_rebind_or_termination(
        &self,
        binding: &mut watch::Receiver<BindingState<M>>,
        stale: &MailboxRef<M>,
    ) -> Result<(), SendError> {
        loop {
            match binding.borrow().clone() {
                BindingState::Bound(current) if !current.same_channel(stale) => return Ok(()),
                BindingState::Bound(_) | BindingState::Unbound => {}
                BindingState::Terminated => return Err(self.actor_terminated()),
            }

            binding
                .changed()
                .await
                .map_err(|_| self.actor_terminated())?;
        }
    }

    fn actor_terminated(&self) -> SendError {
        SendError::ActorTerminated {
            actor_id: self.actor_id.to_string(),
        }
    }

    fn observe_send(&self, operation: MessageOperation, rejection: Option<SendRejection>) {
        trace_actor_message(
            self.source_actor_id.as_deref(),
            &self.actor_id,
            operation,
            rejection,
        );
    }

    fn record_message_size(&self, message_size: Option<usize>) {
        if let Some(message_size) = message_size {
            self.stats.record_message_size(message_size);
            self.message_size
                .as_ref()
                .expect("message size was produced by an observer")
                .record_metrics(message_size);
        }
    }

    pub(crate) fn record_received(&self) {
        self.stats.record_received();
    }
}

/// One-shot reply channel carried inside a request message.
///
/// Created by [`ActorRef::call`]; the receiving actor answers with
/// [`Reply::send`]. Dropping a `Reply` without sending makes the caller's
/// `call` fail with [`CallError::ReplyDropped`].
pub struct Reply<T> {
    sender: oneshot::Sender<T>,
}

/// Handle for a message timer created by [`ActorContext`].
///
/// Clones refer to the same timer. Dropping the handle does not cancel the
/// timer; call [`cancel`](Self::cancel) explicitly. Timers are also cancelled
/// automatically when their actor incarnation stops or restarts.
#[derive(Clone, Debug)]
pub struct TimerRef {
    cancellation: CancellationToken,
}

impl TimerRef {
    /// Cancels the timer.
    ///
    /// Cancellation is idempotent. A message already accepted by the mailbox
    /// cannot be retracted.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Returns `true` if this timer has been cancelled.
    ///
    /// Completion is distinct from cancellation: a one-shot timer that has
    /// already fired is not considered cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

impl<T> Reply<T> {
    /// Sends the reply to the caller.
    ///
    /// If the caller has gone away the value is dropped silently.
    pub fn send(self, value: T) {
        let _ = self.sender.send(value);
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply").finish_non_exhaustive()
    }
}

/// Runtime context passed to a graph actor each time the graph is run.
///
/// Provides the actor's incoming [`mailbox`](Self::recv), message
/// [`timers`](Self::send_after), a
/// [`shutdown_token`](Self::shutdown_token) for cooperative shutdown, and
/// [`run_blocking`](Self::run_blocking) for blocking work.
pub struct ActorContext<M> {
    pub(crate) id: Arc<str>,
    pub(crate) mailbox: MailboxReceiver<M>,
    pub(crate) myself: ActorRef<M>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) observability: GraphObservability,
    pub(crate) timers: ActorTimers,
    pub(crate) state_timeout: Mutex<Option<TimerRef>>,
    pub(crate) monitors: ActorMonitors,
    pub(crate) ready: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) continuations: Mutex<VecDeque<M>>,
}

impl<M: Send + 'static> ActorContext<M> {
    /// Reports that a custom [`RawActor`](crate::RawActor) has completed
    /// initialization.
    ///
    /// This is only needed when `RawActor::readiness_gated` is overridden to
    /// return `true`. Handler-style [`Actor`](crate::Actor) implementations
    /// report readiness automatically after `on_start` succeeds.
    pub fn mark_ready(&self) {
        if let Some(ready) = self
            .ready
            .lock()
            .expect("actor ready signal poisoned")
            .take()
        {
            let _ = ready.send(());
        }
    }

    pub(crate) fn take_continuation(&self) -> Option<M> {
        self.continuations
            .lock()
            .expect("actor continuation queue poisoned")
            .pop_front()
    }

    /// Queues initialization follow-up work as the actor's next message.
    ///
    /// Calls made from [`Actor::on_start`](crate::Actor::on_start) are
    /// processed after startup readiness is reported and before ordinary
    /// mailbox messages. This keeps expensive warm-up work out of the
    /// readiness-critical initialization path. Continuations count as
    /// received messages in [`ActorStats`](crate::ActorStats), but not as
    /// externally accepted mailbox messages.
    pub fn continue_with(&self, message: M) {
        self.continuations
            .lock()
            .expect("actor continuation queue poisoned")
            .push_back(message);
    }

    /// Returns the actor's unique identifier within the graph.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the shared graph shutdown token.
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Returns `true` if graph shutdown has been requested.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Returns a sender targeting this actor's own mailbox.
    pub fn myself(&self) -> ActorRef<M> {
        self.myself.clone()
    }

    /// Watches the target logical actor across restarts.
    ///
    /// Each lifecycle transition of the target is converted by `map` into
    /// this actor's message type and delivered through this actor's mailbox,
    /// in lifecycle order: [`MonitorEvent::Up`] when an incarnation starts,
    /// [`MonitorEvent::Down`] when it exits, and a final
    /// [`MonitorEvent::Terminated`] when the target is permanently gone. A
    /// target that is already running delivers an immediate `Up` for the
    /// current incarnation; a target between incarnations stays silent until
    /// the next start, so a watch never races a supervisor restart. Watches
    /// are automatically cancelled when this observer incarnation stops or
    /// restarts, so register them on start.
    ///
    /// Delivery uses the observer's ordinary mailbox policy. A conflating
    /// mailbox may replace an unread event with a later one, so use a FIFO
    /// mailbox when every transition must be observed. Undelivered events are
    /// staged in a bounded per-watch buffer, so an observer whose mailbox
    /// stays full while its target restarts in a tight loop cannot grow memory
    /// without bound. On overflow the oldest transitions are dropped and the
    /// loss surfaces as a [`MonitorEvent::Lagged`] resync marker rather than
    /// silently; the terminal `Terminated` is never dropped.
    pub fn watch<T, F>(&self, target: &ActorRef<T>, mut map: F) -> MonitorRef
    where
        T: Send + 'static,
        F: FnMut(MonitorEvent) -> M + Send + 'static,
    {
        let cancellation = self.monitors.child_token();
        let monitor = MonitorRef {
            cancellation: cancellation.clone(),
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return monitor;
        };
        // The guard closes the queue on drop, so the hub stops staging events
        // whether this task exits normally or unwinds through a panicking
        // `map` closure.
        let guard = target.monitors.register_watch(cancellation.clone());
        let myself = self.myself();
        runtime.spawn(async move {
            loop {
                // Arm the wake-up before observing the queue so a push that
                // races an empty drain is not lost.
                let waiter = guard.queue().waiter();
                if let Some(event) = guard.queue().pop() {
                    let terminal = matches!(event, MonitorEvent::Terminated { .. });
                    let message = map(event);
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => break,
                        _ = myself.send(message) => {}
                    }
                    if terminal {
                        break;
                    }
                    continue;
                }
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => break,
                    _ = waiter => {}
                }
            }
        });

        monitor
    }

    /// Sends `message` to this actor after `delay` has elapsed.
    ///
    /// Delivery uses the actor's ordinary mailbox policy: FIFO queues wait for
    /// capacity, while conflating mailboxes replace stale unread state. The
    /// timer is cancelled automatically if this actor incarnation stops or
    /// restarts. To schedule delayed delivery to another actor, use
    /// [`send_after_to`](Self::send_after_to).
    pub fn send_after(&self, message: M, delay: Duration) -> TimerRef {
        self.send_after_to(&self.myself, message, delay)
    }

    /// Sends `message` to `target` after `delay` has elapsed.
    ///
    /// The timer is bound to the lifecycle of the *scheduling* actor, exactly
    /// like [`send_after`](Self::send_after): it is cancelled automatically
    /// when this actor incarnation stops or restarts. It is not bound to the
    /// target's lifecycle — if the target restarts before the timer fires,
    /// the message is delivered to whichever target incarnation is running at
    /// fire time, so messages should carry enough context (a key or
    /// generation) for the handler to reject ones it no longer expects.
    /// Delivery uses the target's ordinary mailbox policy: FIFO queues wait
    /// for capacity, while conflating mailboxes replace stale unread state.
    pub fn send_after_to<T: Send + 'static>(
        &self,
        target: &ActorRef<T>,
        message: T,
        delay: Duration,
    ) -> TimerRef {
        let cancellation = self.timers.child_token();
        let timer = TimerRef {
            cancellation: cancellation.clone(),
        };
        spawn_delayed_send(target.clone(), message, delay, cancellation);

        timer
    }

    /// Sends `message` after `delay`, replacing the current state timeout.
    ///
    /// Scheduling a new state timeout cancels the previous one. Call this when
    /// entering a timed state, and call [`clear_state_timeout`](Self::clear_state_timeout)
    /// when entering an untimed state. Like other timers, the timeout is
    /// cancelled automatically if this actor incarnation stops or restarts.
    ///
    /// Cancellation cannot retract a timeout message already accepted by the
    /// mailbox. The message should identify the state (or a generation) it was
    /// scheduled for so the handler can reject stale timeouts. Replacing or
    /// clearing the slot cancels its token even if it already fired, so a
    /// retained handle will then report [`TimerRef::is_cancelled`] as `true`.
    pub fn state_timeout(&self, message: M, delay: Duration) -> TimerRef {
        let cancellation = self.timers.child_token();
        let timer = TimerRef {
            cancellation: cancellation.clone(),
        };
        let previous = self
            .state_timeout
            .lock()
            .expect("state timeout lock poisoned")
            .replace(timer.clone());
        if let Some(previous) = previous {
            previous.cancel();
        }
        spawn_delayed_send(self.myself(), message, delay, cancellation);

        timer
    }

    /// Cancels and clears the current state timeout, if any.
    ///
    /// Call this when entering a state that has no timeout. A timeout message
    /// already accepted by the mailbox cannot be retracted.
    pub fn clear_state_timeout(&self) {
        let timeout = self
            .state_timeout
            .lock()
            .expect("state timeout lock poisoned")
            .take();
        if let Some(timeout) = timeout {
            timeout.cancel();
        }
    }

    /// Sends a clone of `message` to this actor after every `period`.
    ///
    /// The first message is sent after one full period. FIFO delivery waits
    /// for mailbox capacity; conflating delivery replaces stale unread state.
    /// Missed ticks are skipped rather than accumulated. The timer stops on
    /// cancellation, delivery failure, or when this actor incarnation stops or
    /// restarts. To schedule periodic delivery to another actor, use
    /// [`interval_to`](Self::interval_to).
    ///
    /// # Panics
    ///
    /// Panics if `period` is zero.
    pub fn interval(&self, message: M, period: Duration) -> TimerRef
    where
        M: Clone,
    {
        self.interval_to(&self.myself, message, period)
    }

    /// Sends a clone of `message` to `target` after every `period`.
    ///
    /// The first message is sent after one full period. Delivery uses the
    /// target's ordinary mailbox policy — FIFO queues wait for capacity,
    /// conflating mailboxes replace stale unread state — and missed ticks are
    /// skipped rather than accumulated. Like
    /// [`send_after_to`](Self::send_after_to), the timer is bound to the
    /// lifecycle of the *scheduling* actor, not the target's: it stops on
    /// cancellation, when this actor incarnation stops or restarts, or on
    /// delivery failure (the target has permanently terminated). A target
    /// that merely restarts does not stop the timer; later ticks are
    /// delivered to its next incarnation.
    ///
    /// # Panics
    ///
    /// Panics if `period` is zero.
    pub fn interval_to<T>(&self, target: &ActorRef<T>, message: T, period: Duration) -> TimerRef
    where
        T: Clone + Send + 'static,
    {
        assert!(!period.is_zero(), "timer period must be non-zero");

        let cancellation = self.timers.child_token();
        let timer = TimerRef {
            cancellation: cancellation.clone(),
        };
        let target = target.clone();

        tokio::spawn(async move {
            let start = Instant::now() + period;
            let mut interval = tokio::time::interval_at(start, period);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => break,
                    _ = interval.tick() => {
                        let sent = tokio::select! {
                            biased;
                            () = cancellation.cancelled() => break,
                            sent = target.send(message.clone()) => sent,
                        };
                        if sent.is_err() {
                            cancellation.cancel();
                            break;
                        }
                    }
                }
            }
        });

        timer
    }

    /// Waits for the next mailbox message, or `None` once shutdown has been
    /// requested or the mailbox has been closed.
    ///
    /// Shutdown is checked first: as soon as shutdown is requested this
    /// returns `None`, even when messages are still queued. Queued messages
    /// are dropped when the actor exits unless the actor drains them with
    /// [`try_recv`](Self::try_recv), or uses [`Actor`](crate::Actor)
    /// with [`DrainPolicy::Drain`](crate::DrainPolicy). Queued
    /// [`call`](ActorRef::call)s whose reply messages are dropped observe
    /// [`CallError::ReplyDropped`](crate::CallError::ReplyDropped).
    pub async fn recv(&mut self) -> Option<M> {
        let message = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => None,
            message = self.mailbox.recv() => message,
        };

        if message.is_some() {
            self.myself.record_received();
            self.observability.emit_message_received(&self.id);
        }

        message
    }

    /// Attempts to receive a queued message without waiting and without
    /// consulting the shutdown token.
    ///
    /// This is intended for drain-then-exit loops in hand-written
    /// [`RawActor::run`](crate::RawActor::run) implementations: after
    /// [`recv`](Self::recv) returns `None` because shutdown was requested,
    /// queued messages remain readable here.
    ///
    /// A returned [`TryRecvError::Empty`] means no message is immediately
    /// available; it does not prove the mailbox is fully drained while senders
    /// hold permits. For typical actors, prefer
    /// [`Actor`](crate::Actor) with
    /// [`DrainPolicy::Drain`](crate::DrainPolicy) so the framework owns the
    /// drain loop.
    pub fn try_recv(&mut self) -> Result<M, TryRecvError> {
        let message = self.mailbox.try_recv().map_err(|error| match error {
            tokio::sync::mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::mpsc::error::TryRecvError::Disconnected => TryRecvError::Disconnected,
        });
        if message.is_ok() {
            self.myself.record_received();
            self.observability.emit_message_received(&self.id);
        }
        message
    }

    /// Runs blocking work on Tokio's blocking pool and waits for its result.
    ///
    /// The closure receives a child of this actor's shutdown token. The token
    /// is also cancelled if the `run_blocking` future is dropped. Cancellation
    /// is cooperative: long-running closures should check
    /// [`CancellationToken::is_cancelled`] periodically and return promptly.
    ///
    /// A panic in the closure resumes on the actor task, so supervision treats
    /// it as an ordinary actor panic. The return value is otherwise opaque to
    /// the framework; use your own `Result` type when blocking work can fail.
    ///
    /// The actor's configured
    /// [`actor_shutdown_timeout`](crate::GraphBuilder::actor_shutdown_timeout)
    /// is the backstop for closures that ignore cancellation. Once that timeout
    /// aborts the actor task, the blocking thread continues detached because
    /// Tokio blocking tasks cannot be aborted after they start.
    ///
    /// For detached or concurrent work, clone [`myself`](Self::myself), call
    /// [`tokio::task::spawn_blocking`] directly, and send the outcome back as a
    /// message. The mailbox then acts as the completion mechanism; see the
    /// [`blocking_lifecycle` example](https://github.com/ralexstokes/tokio-otp/blob/main/crates/tokio-otp/examples/blocking_lifecycle.rs).
    pub async fn run_blocking<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&CancellationToken) -> R + Send + 'static,
        R: Send + 'static,
    {
        let cancellation = self.shutdown.child_token();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let joined = tokio::task::spawn_blocking(move || f(&cancellation)).await;

        match joined {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("blocking task was cancelled: {error}"),
        }
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn spawn_delayed_send<T: Send + 'static>(
    target: ActorRef<T>,
    message: T,
    delay: Duration,
    cancellation: CancellationToken,
) {
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {}
            () = tokio::time::sleep(delay) => {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {}
                    _ = target.send(message) => {}
                }
            }
        }
    });
}

pub(crate) struct ActorTimers(CancellationToken);

impl ActorTimers {
    pub(crate) fn new(shutdown: &CancellationToken) -> Self {
        Self(shutdown.child_token())
    }

    fn child_token(&self) -> CancellationToken {
        self.0.child_token()
    }
}

impl Drop for ActorTimers {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn send_rejection(error: &SendError) -> SendRejection {
    match error {
        SendError::ActorNotRunning { .. } => SendRejection::NotRunning,
        SendError::ActorTerminated { .. } => SendRejection::ActorTerminated,
        SendError::MailboxFull { .. } => SendRejection::MailboxFull,
        SendError::MailboxClosed { .. } => SendRejection::MailboxClosed,
    }
}
