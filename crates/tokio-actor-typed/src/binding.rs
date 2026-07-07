//! Restart-stable mailbox bindings, generalized over the message type.
//!
//! This mirrors `tokio-actor/src/binding.rs` with `Envelope` replaced by a
//! generic `M`. The watch-channel rebinding machinery is unchanged.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::error::SendError;

/// Sender for one bound mailbox instance of an actor.
pub(crate) struct MailboxRef<M> {
    actor_id: Arc<str>,
    sender: mpsc::Sender<M>,
}

impl<M> Clone for MailboxRef<M> {
    fn clone(&self) -> Self {
        Self {
            actor_id: Arc::clone(&self.actor_id),
            sender: self.sender.clone(),
        }
    }
}

impl<M> MailboxRef<M> {
    pub(crate) fn new(actor_id: Arc<str>, sender: mpsc::Sender<M>) -> Self {
        Self { actor_id, sender }
    }

    /// Sends, returning the message on failure so callers can retry after a
    /// rebind.
    pub(crate) async fn send_retaining(&self, message: M) -> Result<(), M> {
        self.sender.send(message).await.map_err(|err| err.0)
    }

    pub(crate) async fn send(&self, message: M) -> Result<(), SendError> {
        self.send_retaining(message)
            .await
            .map_err(|_| SendError::MailboxClosed {
                actor_id: self.actor_id.to_string(),
            })
    }

    pub(crate) fn try_send(&self, message: M) -> Result<(), SendError> {
        self.sender.try_send(message).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => SendError::MailboxFull {
                actor_id: self.actor_id.to_string(),
            },
            mpsc::error::TrySendError::Closed(_) => SendError::MailboxClosed {
                actor_id: self.actor_id.to_string(),
            },
        })
    }

    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }
}

/// Long-lived binding slot for one actor's current mailbox.
///
/// [`ActorRef`](crate::ActorRef)s subscribe to this slot, so they
/// transparently follow the current mailbox across graph reruns.
pub(crate) struct BindingCore<M> {
    actor_id: Arc<str>,
    current: watch::Sender<Option<MailboxRef<M>>>,
}

impl<M> BindingCore<M> {
    pub(crate) fn new(actor_id: Arc<str>) -> Self {
        let (current, _receiver) = watch::channel(None);
        Self { actor_id, current }
    }

    pub(crate) fn actor_id(&self) -> &Arc<str> {
        &self.actor_id
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<MailboxRef<M>>> {
        self.current.subscribe()
    }

    fn bind(&self, mailbox: MailboxRef<M>) {
        self.current.send_replace(Some(mailbox));
    }

    fn clear(&self) {
        self.current.send_replace(None);
    }
}

/// Binds a mailbox on creation and clears the binding when the actor's run
/// ends.
pub(crate) struct BindingGuard<M> {
    core: Arc<BindingCore<M>>,
}

impl<M> BindingGuard<M> {
    pub(crate) fn bind(core: Arc<BindingCore<M>>, mailbox: MailboxRef<M>) -> Self {
        core.bind(mailbox);
        Self { core }
    }
}

impl<M> Drop for BindingGuard<M> {
    fn drop(&mut self) {
        self.core.clear();
    }
}
