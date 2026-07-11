use std::{fmt, sync::Arc};

use tokio::sync::{
    mpsc::{self, error::TryRecvError},
    oneshot, watch,
};
use tokio_util::sync::CancellationToken;

use crate::actor::{
    binding::{
        ActorStats, ActorStatsCounters, BindingCore, BindingState, MailboxRef, MessageSizeObserver,
    },
    builder::MessageSize,
    error::{CallError, SendError},
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
}

impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        Self {
            actor_id: Arc::clone(&self.actor_id),
            binding: self.binding.clone(),
            stats: Arc::clone(&self.stats),
            message_size: self.message_size.clone(),
            source_actor_id: self.source_actor_id.clone(),
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
        )
    }

    pub(crate) fn from_parts(
        actor_id: Arc<str>,
        binding: watch::Receiver<BindingState<M>>,
        stats: Arc<ActorStatsCounters>,
        message_size: Option<Arc<MessageSizeObserver<M>>>,
        source_actor_id: Option<Arc<str>>,
    ) -> Self {
        Self {
            actor_id,
            binding,
            stats,
            message_size,
            source_actor_id,
        }
    }

    pub(crate) fn detached(actor_id: Arc<str>) -> Self {
        let core = Arc::new(BindingCore::<M>::new(actor_id));
        Self::from_core(&core, None)
    }

    pub(crate) fn detached_with_message_size(actor_id: Arc<str>) -> Self
    where
        M: MessageSize,
    {
        let core = Arc::new(BindingCore::<M>::with_message_size(
            actor_id,
            MessageSize::size_hint,
        ));
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
    /// This waits until the actor has a bound mailbox, waits for mailbox
    /// capacity, and rides through restart windows when the actor is expected
    /// to rebind. It returns an error only when the actor has terminated with
    /// no restart scheduled, or when the binding source has been dropped.
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
                Ok(()) => {
                    self.observe_send(MessageOperation::Send, None);
                    self.stats.record_send(true);
                    self.record_message_size(message_size);
                    return Ok(());
                }
                Err(returned) => {
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
        if result.is_ok() {
            self.record_message_size(message_size);
        }
        result
    }

    /// Sends a request and waits for the actor to answer through the
    /// [`Reply`] carried inside the message.
    ///
    /// This waits for the same actor binding conditions as [`send`](Self::send).
    /// Cancelling this future while it is waiting drops the request message.
    ///
    /// ```ignore
    /// enum Msg { Get(Reply<u64>) }
    /// let value = actor_ref.call(Msg::Get).await?;
    /// ```
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
/// Provides the actor's incoming [`mailbox`](Self::recv), a
/// [`shutdown_token`](Self::shutdown_token) for cooperative shutdown, and
/// [`run_blocking`](Self::run_blocking) for blocking work.
pub struct ActorContext<M> {
    pub(crate) id: Arc<str>,
    pub(crate) mailbox: mpsc::Receiver<M>,
    pub(crate) myself: ActorRef<M>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) observability: GraphObservability,
}

impl<M: Send + 'static> ActorContext<M> {
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
        let message = self.mailbox.try_recv();
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

fn send_rejection(error: &SendError) -> SendRejection {
    match error {
        SendError::ActorNotRunning { .. } => SendRejection::NotRunning,
        SendError::ActorTerminated { .. } => SendRejection::ActorTerminated,
        SendError::MailboxFull { .. } => SendRejection::MailboxFull,
        SendError::MailboxClosed { .. } => SendRejection::MailboxClosed,
    }
}
