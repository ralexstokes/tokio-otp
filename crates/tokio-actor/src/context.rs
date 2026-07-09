use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use tokio::sync::{
    mpsc::{self, error::TryRecvError},
    oneshot, watch,
};
use tokio_util::sync::CancellationToken;

use crate::{
    binding::{BindingCore, BindingState, MailboxRef},
    blocking::{
        BlockingContext, BlockingHandle, BlockingOperationError, BlockingOptions, BlockingSpawner,
        BlockingTaskError, SpawnBlockingError,
    },
    error::{CallError, SendError},
    observability::{GraphObservability, MessageOperation, SendRejection},
    registry::ActorRegistry,
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
    observability: Arc<OnceLock<GraphObservability>>,
    source_actor_id: Option<Arc<str>>,
}

impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        Self {
            actor_id: Arc::clone(&self.actor_id),
            binding: self.binding.clone(),
            observability: Arc::clone(&self.observability),
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
            core.observability_slot(),
            source_actor_id,
        )
    }

    pub(crate) fn from_parts(
        actor_id: Arc<str>,
        binding: watch::Receiver<BindingState<M>>,
        observability: Arc<OnceLock<GraphObservability>>,
        source_actor_id: Option<Arc<str>>,
    ) -> Self {
        Self {
            actor_id,
            binding,
            observability,
            source_actor_id,
        }
    }

    pub(crate) fn detached(actor_id: Arc<str>) -> Self {
        let core = Arc::new(BindingCore::<M>::new(actor_id));
        Self::from_core(&core, None)
    }

    /// Returns the target actor id.
    pub fn id(&self) -> &str {
        &self.actor_id
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
    pub async fn send(&self, message: M) -> Result<(), SendError> {
        let mut binding = self.binding.clone();
        let mut message = message;

        loop {
            let started_at = self.start_message_timing();
            let mailbox = match self.wait_for_next_mailbox(&mut binding).await {
                Ok(mailbox) => mailbox,
                Err(error) => {
                    let result = Err(error);
                    self.observe_send(
                        MessageOperation::Send,
                        Self::finish_message_timing(started_at),
                        &result,
                    );
                    return result;
                }
            };

            match mailbox.send_retaining(message).await {
                Ok(()) => {
                    let result = Ok(());
                    self.observe_send(
                        MessageOperation::Send,
                        Self::finish_message_timing(started_at),
                        &result,
                    );
                    return result;
                }
                Err(returned) => {
                    let result = Err(SendError::MailboxClosed {
                        actor_id: self.actor_id.to_string(),
                    });
                    self.observe_send(
                        MessageOperation::Send,
                        Self::finish_message_timing(started_at),
                        &result,
                    );
                    message = returned;
                    self.wait_for_rebind_or_termination(&mut binding, &mailbox)
                        .await?;
                }
            }
        }
    }

    /// Attempts to send a message without waiting for mailbox capacity.
    pub fn try_send(&self, message: M) -> Result<(), SendError> {
        let started_at = self.start_message_timing();
        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox.try_send(message),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::TrySend,
            Self::finish_message_timing(started_at),
            &result,
        );
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

    fn start_message_timing(&self) -> Option<std::time::Instant> {
        self.observability
            .get()
            .and_then(GraphObservability::start_message_timing)
    }

    fn finish_message_timing(started_at: Option<std::time::Instant>) -> std::time::Duration {
        GraphObservability::finish_message_timing(started_at)
    }

    fn observe_send(
        &self,
        operation: MessageOperation,
        duration: std::time::Duration,
        result: &Result<(), SendError>,
    ) {
        if let Some(observability) = self.observability.get() {
            observability.emit_actor_message(
                self.source_actor_id.as_deref(),
                &self.actor_id,
                operation,
                duration,
                result.as_ref().err().map(send_rejection),
            );
        }
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
/// Provides the actor's incoming [`mailbox`](Self::recv), an optional
/// [`registry`](Self::registry) for runtime-discovered actors, a
/// [`shutdown_token`](Self::shutdown_token) for cooperative shutdown, and
/// [`spawn_blocking`](Self::spawn_blocking) /
/// [`run_blocking`](Self::run_blocking) for tracked blocking work.
pub struct ActorContext<M> {
    pub(crate) id: Arc<str>,
    pub(crate) mailbox: mpsc::Receiver<M>,
    pub(crate) registry: Option<ActorRegistry>,
    pub(crate) myself: ActorRef<M>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) blocking: BlockingSpawner<M>,
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

    /// Returns the optional runtime actor registry.
    pub fn registry(&self) -> Option<&ActorRegistry> {
        self.registry.as_ref()
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
    /// [`try_recv`](Self::try_recv), or uses [`MessageHandler`](crate::MessageHandler)
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
            self.observability.emit_message_received(&self.id);
        }

        message
    }

    /// Attempts to receive a queued message without waiting and without
    /// consulting the shutdown token.
    ///
    /// This is intended for drain-then-exit loops in hand-written
    /// [`Actor::run`](crate::Actor::run) implementations: after
    /// [`recv`](Self::recv) returns `None` because shutdown was requested,
    /// queued messages remain readable here.
    ///
    /// A returned [`TryRecvError::Empty`] means no message is immediately
    /// available; it does not prove the mailbox is fully drained while senders
    /// hold permits. For typical actors, prefer
    /// [`MessageHandler`](crate::MessageHandler) with
    /// [`DrainPolicy::Drain`](crate::DrainPolicy) so the framework owns the
    /// drain loop.
    pub fn try_recv(&mut self) -> Result<M, TryRecvError> {
        let message = self.mailbox.try_recv();
        if message.is_ok() {
            self.observability.emit_message_received(&self.id);
        }
        message
    }

    /// Spawns tracked blocking work owned by this actor.
    ///
    /// If the returned handle is dropped without being awaited, non-cancelled
    /// task failures are treated as actor failures. New work is rejected once
    /// the actor reaches its configured blocking-task limit. Blocking closures
    /// should check [`BlockingContext::checkpoint`] or
    /// [`BlockingContext::is_cancelled`] regularly when graceful shutdown
    /// matters.
    pub fn spawn_blocking<F>(
        &self,
        options: BlockingOptions,
        f: F,
    ) -> Result<BlockingHandle, SpawnBlockingError>
    where
        F: FnOnce(BlockingContext<M>) -> Result<(), BlockingOperationError> + Send + 'static,
    {
        self.blocking.spawn_blocking(options, f)
    }

    /// Runs tracked blocking work and waits for it to finish.
    ///
    /// Failures returned from this method are considered handled by the
    /// caller and do not also fail the actor implicitly. If the blocking
    /// closure ignores cancellation, awaiting this method remains pending until
    /// the closure returns.
    pub async fn run_blocking<F>(
        &self,
        options: BlockingOptions,
        f: F,
    ) -> Result<(), BlockingTaskError>
    where
        F: FnOnce(BlockingContext<M>) -> Result<(), BlockingOperationError> + Send + 'static,
    {
        let handle = self.spawn_blocking(options, f)?;
        handle.wait().await
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
