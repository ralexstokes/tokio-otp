use std::{fmt, sync::Arc};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    binding::MailboxRef,
    error::{CallError, SendError},
};

/// Cloneable, restart-stable, typed sender for an actor mailbox.
///
/// An `ActorRef<M>` is bound to a long-lived mailbox binding rather than a
/// single actor runtime instance. When the target actor is restarted as part
/// of a graph rerun, the handle transparently follows the new mailbox.
pub struct ActorRef<M> {
    actor_id: Arc<str>,
    binding: watch::Receiver<Option<MailboxRef<M>>>,
}

impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        Self {
            actor_id: Arc::clone(&self.actor_id),
            binding: self.binding.clone(),
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
    pub(crate) fn new(actor_id: Arc<str>, binding: watch::Receiver<Option<MailboxRef<M>>>) -> Self {
        Self { actor_id, binding }
    }

    /// Returns the target actor id.
    pub fn id(&self) -> &str {
        &self.actor_id
    }

    fn current_mailbox(&self) -> Result<MailboxRef<M>, SendError> {
        self.binding
            .borrow()
            .clone()
            .ok_or_else(|| SendError::ActorNotRunning {
                actor_id: self.actor_id.to_string(),
            })
    }

    /// Sends a message to the target actor, waiting for mailbox capacity.
    pub async fn send(&self, message: M) -> Result<(), SendError> {
        self.current_mailbox()?.send(message).await
    }

    /// Attempts to send a message without waiting for mailbox capacity.
    pub fn try_send(&self, message: M) -> Result<(), SendError> {
        self.current_mailbox()?.try_send(message)
    }

    /// Retries a send across transient restart windows until the actor is
    /// rebound or the binding source is dropped.
    pub async fn send_when_ready(&mut self, message: M) -> Result<(), SendError> {
        let mut message = message;
        loop {
            match self.current_mailbox() {
                Ok(mailbox) => match mailbox.send_retaining(message).await {
                    Ok(()) => return Ok(()),
                    Err(returned) => {
                        message = returned;
                        if !self.wait_for_rebind(&mailbox).await {
                            return Err(SendError::MailboxClosed {
                                actor_id: self.actor_id.to_string(),
                            });
                        }
                    }
                },
                Err(error) => {
                    if !self.wait_for_next_binding().await {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Sends a request and waits for the actor to answer through the
    /// [`Reply`] carried inside the message.
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

    /// Waits until the actor is bound to a running mailbox.
    ///
    /// Returns immediately with `true` if the actor is already running.
    /// Returns `false` if the binding source has been dropped (the graph no
    /// longer exists).
    pub async fn wait_for_binding(&mut self) -> bool {
        self.wait_for_next_binding().await
    }

    async fn wait_for_next_binding(&mut self) -> bool {
        self.binding.wait_for(Option::is_some).await.is_ok()
    }

    /// Waits until the stale mailbox is unbound and a fresh one is bound.
    ///
    /// Waiting for the stale mailbox to clear first avoids busy-looping in
    /// the window where an actor's mailbox is already closed but its binding
    /// has not been cleared yet.
    async fn wait_for_rebind(&mut self, stale: &MailboxRef<M>) -> bool {
        if self
            .binding
            .wait_for(|slot| !matches!(slot, Some(current) if current.same_channel(stale)))
            .await
            .is_err()
        {
            return false;
        }
        self.wait_for_next_binding().await
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
pub struct ActorContext<M> {
    pub(crate) id: Arc<str>,
    pub(crate) mailbox: mpsc::Receiver<M>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) myself: ActorRef<M>,
}

impl<M> ActorContext<M> {
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
    pub async fn recv(&mut self) -> Option<M> {
        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => None,
            message = self.mailbox.recv() => message,
        }
    }
}
