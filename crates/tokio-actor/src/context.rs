use std::{collections::HashMap, sync::Arc};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    binding::{MailboxError, MailboxRef, MailboxSendFailure},
    blocking::{
        BlockingContext, BlockingHandle, BlockingOperationError, BlockingOptions, BlockingSpawner,
        SpawnBlockingError,
    },
    envelope::Envelope,
    error::SendError,
    observability::{GraphObservability, MessageOperation, SendRejection},
    registry::ActorRegistry,
};

/// Cloneable, restart-stable sender for an actor mailbox.
///
/// An `ActorRef` is bound to a long-lived mailbox binding rather than a single
/// actor runtime instance. When the target actor is restarted (either as part
/// of a graph rerun or via per-actor supervision), the handle transparently
/// follows the new mailbox.
#[derive(Clone, Debug)]
pub struct ActorRef {
    actor_id: Arc<str>,
    binding: watch::Receiver<Option<MailboxRef>>,
    observability: GraphObservability,
    source_actor_id: Option<Arc<str>>,
}

impl ActorRef {
    pub(crate) fn from_binding(
        actor_id: Arc<str>,
        binding: watch::Receiver<Option<MailboxRef>>,
        observability: GraphObservability,
        source_actor_id: Option<Arc<str>>,
    ) -> Self {
        Self {
            actor_id,
            binding,
            observability,
            source_actor_id,
        }
    }

    fn current_mailbox(&self) -> Result<MailboxRef, SendError> {
        self.binding
            .borrow()
            .clone()
            .ok_or_else(|| SendError::ActorNotRunning {
                actor_id: self.actor_id.to_string(),
            })
    }

    async fn wait_for_next_binding(&mut self) -> bool {
        self.binding.wait_for(|slot| slot.is_some()).await.is_ok()
    }

    async fn wait_for_binding_change_from(&mut self, mailbox: &MailboxRef) -> bool {
        loop {
            let unchanged = self
                .binding
                .borrow_and_update()
                .as_ref()
                .is_some_and(|current| current.same_channel(mailbox));
            if !unchanged {
                return true;
            }
            if self.binding.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Returns the target actor id.
    pub fn id(&self) -> &str {
        &self.actor_id
    }

    /// Sends an envelope to the target actor, waiting for mailbox capacity.
    pub async fn send(&self, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();
        let started_at = self.observability.start_message_timing();
        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox
                .send(envelope)
                .await
                .map_err(MailboxError::into_send_error),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::Send,
            envelope_len,
            GraphObservability::finish_message_timing(started_at),
            &result,
        );
        result
    }

    /// Retries a send across transient restart windows until the actor is
    /// rebound or the binding source is dropped.
    pub async fn send_when_ready(
        &mut self,
        envelope: impl Into<Envelope>,
    ) -> Result<(), SendError> {
        let mut envelope = envelope.into();

        loop {
            let envelope_len = envelope.as_slice().len();
            let started_at = self.observability.start_message_timing();

            match self.current_mailbox() {
                Ok(mailbox) => match mailbox.send_retaining(envelope).await {
                    Ok(()) => {
                        let result = Ok(());
                        self.observe_send(
                            MessageOperation::Send,
                            envelope_len,
                            GraphObservability::finish_message_timing(started_at),
                            &result,
                        );
                        return result;
                    }
                    Err(MailboxSendFailure::EnvelopeTooLarge {
                        envelope_len,
                        max_envelope_bytes,
                        ..
                    }) => {
                        let error = SendError::EnvelopeTooLarge {
                            actor_id: self.actor_id.to_string(),
                            envelope_len,
                            max_envelope_bytes,
                        };
                        let result = Err(error.clone());
                        self.observe_send(
                            MessageOperation::Send,
                            envelope_len,
                            GraphObservability::finish_message_timing(started_at),
                            &result,
                        );
                        return Err(error);
                    }
                    Err(MailboxSendFailure::MailboxClosed {
                        envelope: returned, ..
                    }) => {
                        let error = SendError::MailboxClosed {
                            actor_id: self.actor_id.to_string(),
                        };
                        let result = Err(error.clone());
                        self.observe_send(
                            MessageOperation::Send,
                            envelope_len,
                            GraphObservability::finish_message_timing(started_at),
                            &result,
                        );
                        envelope = returned;
                        if !self.wait_for_binding_change_from(&mailbox).await {
                            return Err(error);
                        }
                    }
                },
                Err(error) => {
                    let result = Err(error.clone());
                    self.observe_send(
                        MessageOperation::Send,
                        envelope_len,
                        GraphObservability::finish_message_timing(started_at),
                        &result,
                    );
                    if !self.wait_for_next_binding().await {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Attempts to send an envelope without waiting for mailbox capacity.
    pub fn try_send(&self, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();
        let started_at = self.observability.start_message_timing();
        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox
                .try_send(envelope)
                .map_err(MailboxError::into_send_error),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::TrySend,
            envelope_len,
            GraphObservability::finish_message_timing(started_at),
            &result,
        );
        result
    }

    /// Sends an envelope from blocking code without requiring an async context.
    ///
    /// This returns [`SendError::MailboxFull`] instead of blocking the thread
    /// when the mailbox is at capacity. Blocking callers that want to retry
    /// should do so explicitly and check for cancellation between attempts.
    pub fn blocking_send(&self, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();
        let started_at = self.observability.start_message_timing();
        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox
                .blocking_send(envelope)
                .map_err(MailboxError::into_send_error),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::BlockingSend,
            envelope_len,
            GraphObservability::finish_message_timing(started_at),
            &result,
        );
        result
    }

    /// Waits until the actor is bound to a running mailbox.
    ///
    /// Returns immediately if the actor is already running. Returns if the
    /// binding source is dropped.
    pub async fn wait_for_binding(&mut self) {
        let _ = self.wait_for_next_binding().await;
    }

    fn observe_send(
        &self,
        operation: MessageOperation,
        envelope_len: usize,
        duration: std::time::Duration,
        result: &Result<(), SendError>,
    ) {
        self.observability.emit_actor_message(
            self.source_actor_id.as_deref(),
            &self.actor_id,
            operation,
            envelope_len,
            duration,
            result.as_ref().err().map(send_rejection),
        );
    }
}

/// Runtime context passed to a graph actor each time the graph is run.
///
/// Provides the actor's incoming [`mailbox`](Self::recv), stable
/// [`peer`](Self::peer) references for linked actors, an optional
/// [`registry`](Self::registry) for runtime-discovered actors, a
/// [`shutdown_token`](Self::shutdown_token) for cooperative shutdown, and
/// [`spawn_blocking`](Self::spawn_blocking) /
/// [`run_blocking`](Self::run_blocking) for tracked blocking work.
pub struct ActorContext {
    pub(crate) id: Arc<str>,
    pub(crate) mailbox: mpsc::Receiver<Envelope>,
    pub(crate) peers: HashMap<Arc<str>, ActorRef>,
    pub(crate) registry: Option<ActorRegistry>,
    pub(crate) myself: ActorRef,
    pub(crate) shutdown: CancellationToken,
    pub(crate) blocking: BlockingSpawner,
    pub(crate) observability: GraphObservability,
}

impl ActorContext {
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

    /// Waits for the next mailbox message, or `None` once shutdown has been
    /// requested or the mailbox has been closed.
    pub async fn recv(&mut self) -> Option<Envelope> {
        let message = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => None,
            message = self.mailbox.recv() => message,
        };

        if let Some(ref envelope) = message {
            self.observability
                .emit_message_received(&self.id, envelope.as_slice().len());
        }

        message
    }

    /// Returns a linked peer by id.
    pub fn peer(&self, actor_id: &str) -> Option<ActorRef> {
        self.peers.get(actor_id).cloned()
    }

    /// Returns the optional runtime actor registry.
    pub fn registry(&self) -> Option<&ActorRegistry> {
        self.registry.as_ref()
    }

    /// Returns a sender targeting this actor's own mailbox.
    pub fn myself(&self) -> ActorRef {
        self.myself.clone()
    }

    /// Sends an envelope to a linked peer.
    pub async fn send(
        &self,
        actor_id: &str,
        envelope: impl Into<Envelope>,
    ) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();

        match self.peers.get(actor_id) {
            Some(peer) => peer.send(envelope).await,
            None => Err(self.unknown_target(actor_id, MessageOperation::Send, envelope_len)),
        }
    }

    /// Sends an envelope to a linked peer, retrying across restart windows.
    ///
    /// If this actor begins shutting down before the peer becomes ready, the
    /// send is abandoned and this returns `Ok(())` so the actor can exit
    /// promptly.
    pub async fn send_when_ready(
        &self,
        actor_id: &str,
        envelope: impl Into<Envelope>,
    ) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();

        match self.peers.get(actor_id) {
            Some(peer) => {
                let mut peer = peer.clone();
                tokio::select! {
                    _ = self.shutdown.cancelled() => Ok(()),
                    result = peer.send_when_ready(envelope) => result,
                }
            }
            None => Err(self.unknown_target(actor_id, MessageOperation::Send, envelope_len)),
        }
    }

    /// Attempts to send an envelope to a linked peer without waiting.
    pub fn try_send(&self, actor_id: &str, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();

        match self.peers.get(actor_id) {
            Some(peer) => peer.try_send(envelope),
            None => Err(self.unknown_target(actor_id, MessageOperation::TrySend, envelope_len)),
        }
    }

    /// Sends an envelope to a statically linked peer or registry-discovered
    /// actor.
    pub async fn send_dynamic(
        &self,
        actor_id: &str,
        envelope: impl Into<Envelope>,
    ) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();

        if let Some(peer) = self.resolve_dynamic_peer(actor_id) {
            return peer.send(envelope).await;
        }

        Err(self.unknown_target(actor_id, MessageOperation::Send, envelope_len))
    }

    /// Sends an envelope to a statically linked peer or registry-discovered
    /// actor, retrying across restart windows.
    pub async fn send_dynamic_when_ready(
        &self,
        actor_id: &str,
        envelope: impl Into<Envelope>,
    ) -> Result<(), SendError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();

        match self.resolve_dynamic_peer(actor_id) {
            Some(peer) => {
                let mut peer = peer;
                tokio::select! {
                    _ = self.shutdown.cancelled() => Ok(()),
                    result = peer.send_when_ready(envelope) => result,
                }
            }
            None => Err(self.unknown_target(actor_id, MessageOperation::Send, envelope_len)),
        }
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
        F: FnOnce(BlockingContext) -> Result<(), BlockingOperationError> + Send + 'static,
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
    ) -> Result<(), crate::blocking::BlockingTaskError>
    where
        F: FnOnce(BlockingContext) -> Result<(), BlockingOperationError> + Send + 'static,
    {
        let handle = self.spawn_blocking(options, f)?;
        handle.wait().await
    }

    fn resolve_dynamic_peer(&self, actor_id: &str) -> Option<ActorRef> {
        self.peers.get(actor_id).cloned().or_else(|| {
            self.registry
                .as_ref()
                .and_then(|registry| registry.actor_ref_for_source(actor_id, Some(&self.id)))
        })
    }

    fn unknown_target(
        &self,
        actor_id: &str,
        operation: MessageOperation,
        envelope_len: usize,
    ) -> SendError {
        let peer_id: Arc<str> = actor_id.into();
        let started_at = self.observability.start_message_timing();
        self.observability.emit_actor_message(
            Some(self.id()),
            &peer_id,
            operation,
            envelope_len,
            GraphObservability::finish_message_timing(started_at),
            Some(SendRejection::UnknownPeer),
        );

        SendError::UnknownPeer {
            actor_id: self.id.to_string(),
            peer_id: peer_id.to_string(),
        }
    }
}

fn send_rejection(error: &SendError) -> SendRejection {
    match error {
        SendError::UnknownPeer { .. } => SendRejection::UnknownPeer,
        SendError::ActorNotRunning { .. } => SendRejection::NotRunning,
        SendError::EnvelopeTooLarge { .. } => SendRejection::EnvelopeTooLarge,
        SendError::MailboxFull { .. } => SendRejection::MailboxFull,
        SendError::MailboxClosed { .. } => SendRejection::MailboxClosed,
    }
}
