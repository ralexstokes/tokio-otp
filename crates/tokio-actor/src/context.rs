use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    blocking::{
        BlockingContext, BlockingHandle, BlockingOperationError, BlockingOptions, BlockingSpawner,
        SpawnBlockingError,
    },
    envelope::Envelope,
    error::SendError,
    ingress::{MailboxError, MailboxRef},
};

fn map_mailbox_error(err: MailboxError) -> SendError {
    match err {
        MailboxError::MailboxFull { actor_id } => SendError::MailboxFull {
            actor_id: actor_id.to_string(),
        },
        MailboxError::MailboxClosed { actor_id } => SendError::MailboxClosed {
            actor_id: actor_id.to_string(),
        },
    }
}

/// Cloneable sender for an actor mailbox.
#[derive(Clone, Debug)]
pub struct ActorRef {
    mailbox: MailboxRef,
}

impl ActorRef {
    pub(crate) fn from_mailbox(mailbox: MailboxRef) -> Self {
        Self { mailbox }
    }

    /// Returns the target actor id.
    pub fn id(&self) -> &str {
        self.mailbox.actor_id()
    }

    /// Sends an envelope to the target actor, waiting for mailbox capacity.
    pub async fn send(&self, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        self.mailbox
            .send(envelope.into())
            .await
            .map_err(map_mailbox_error)
    }

    /// Attempts to send an envelope without waiting for mailbox capacity.
    pub fn try_send(&self, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        self.mailbox
            .try_send(envelope.into())
            .map_err(map_mailbox_error)
    }

    /// Sends an envelope from blocking code, waiting for mailbox capacity.
    pub fn blocking_send(&self, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        self.mailbox
            .blocking_send(envelope.into())
            .map_err(map_mailbox_error)
    }
}

/// Runtime context passed to a graph actor each time the graph is run.
pub struct ActorContext {
    pub(crate) id: String,
    pub(crate) mailbox: mpsc::Receiver<Envelope>,
    pub(crate) peers: HashMap<String, ActorRef>,
    pub(crate) myself: ActorRef,
    pub(crate) shutdown: CancellationToken,
    pub(crate) blocking: BlockingSpawner,
}

impl ActorContext {
    fn linked_peer(&self, actor_id: &str) -> Result<&ActorRef, SendError> {
        self.peers
            .get(actor_id)
            .ok_or_else(|| SendError::UnknownPeer {
                actor_id: self.id.clone(),
                peer_id: actor_id.to_owned(),
            })
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

    /// Waits for the next mailbox message, or `None` once shutdown has been
    /// requested or the mailbox has been closed.
    pub async fn recv(&mut self) -> Option<Envelope> {
        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => None,
            message = self.mailbox.recv() => message,
        }
    }

    /// Returns a linked peer by id.
    pub fn peer(&self, actor_id: &str) -> Option<ActorRef> {
        self.peers.get(actor_id).cloned()
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
        let peer = self.linked_peer(actor_id)?;
        peer.send(envelope).await
    }

    /// Attempts to send an envelope to a linked peer without waiting.
    pub fn try_send(&self, actor_id: &str, envelope: impl Into<Envelope>) -> Result<(), SendError> {
        let peer = self.linked_peer(actor_id)?;
        peer.try_send(envelope)
    }

    /// Spawns tracked blocking work owned by this actor.
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
}
