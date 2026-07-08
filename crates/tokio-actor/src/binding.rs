use std::sync::{Arc, OnceLock};

use tokio::sync::{mpsc, watch};

use crate::{error::SendError, observability::GraphObservability};

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

/// The current lifecycle state of an actor mailbox binding.
pub(crate) enum BindingState<M> {
    /// Not yet started, or between restarts where a new mailbox is expected.
    Unbound,
    Bound(MailboxRef<M>),
    /// No restart is scheduled. A later graph rerun or actor re-add may bind
    /// a new mailbox.
    Terminated,
}

impl<M> Clone for BindingState<M> {
    fn clone(&self) -> Self {
        match self {
            Self::Unbound => Self::Unbound,
            Self::Bound(mailbox) => Self::Bound(mailbox.clone()),
            Self::Terminated => Self::Terminated,
        }
    }
}

/// Controls whether a binding should wait for another mailbox after a run
/// exits.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RebindPolicy {
    /// A rebind is always expected unless shutdown was requested.
    Always,
    /// A rebind is expected after failure, panic, cancellation, or drop.
    OnFailure,
    /// No rebind is expected.
    #[default]
    Never,
}

pub(crate) trait BindingLifecycle: Send + Sync {
    fn unbind(&self);
    fn terminate(&self);
}

/// Long-lived binding slot for one actor's current mailbox.
///
/// [`ActorRef`](crate::ActorRef)s subscribe to this slot, so they
/// transparently follow the current mailbox across graph reruns and per-actor
/// restarts.
pub(crate) struct BindingCore<M> {
    actor_id: Arc<str>,
    current: watch::Sender<BindingState<M>>,
    observability: Arc<OnceLock<GraphObservability>>,
}

impl<M> BindingCore<M> {
    pub(crate) fn new(actor_id: Arc<str>) -> Self {
        let (current, _receiver) = watch::channel(BindingState::Unbound);
        Self {
            actor_id,
            current,
            observability: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn actor_id(&self) -> &Arc<str> {
        &self.actor_id
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<BindingState<M>> {
        self.current.subscribe()
    }

    pub(crate) fn observability_slot(&self) -> Arc<OnceLock<GraphObservability>> {
        Arc::clone(&self.observability)
    }

    pub(crate) fn set_observability(&self, observability: GraphObservability) {
        let _ = self.observability.set(observability);
    }

    fn bind(&self, mailbox: MailboxRef<M>) {
        self.current.send_replace(BindingState::Bound(mailbox));
    }

    /// Only a bound mailbox can be unbound: once a binding is terminated, a
    /// racing unbind from a late run teardown must not regress it to
    /// `Unbound`, or senders would wait for a rebind that never comes.
    pub(crate) fn unbind(&self) {
        self.current.send_if_modified(|state| {
            if matches!(state, BindingState::Bound(_)) {
                *state = BindingState::Unbound;
                true
            } else {
                false
            }
        });
    }

    pub(crate) fn terminate(&self) {
        self.current.send_replace(BindingState::Terminated);
    }
}

impl<M: Send + 'static> BindingLifecycle for BindingCore<M> {
    fn unbind(&self) {
        BindingCore::unbind(self);
    }

    fn terminate(&self) {
        BindingCore::terminate(self);
    }
}

/// Binds a mailbox on creation and clears the binding when the actor's run
/// ends.
pub(crate) struct BindingGuard<M> {
    core: Arc<BindingCore<M>>,
    observability: GraphObservability,
    rebind_policy: RebindPolicy,
}

impl<M> BindingGuard<M> {
    pub(crate) fn bind(
        core: Arc<BindingCore<M>>,
        mailbox: MailboxRef<M>,
        observability: GraphObservability,
        rebind_policy: RebindPolicy,
    ) -> Self {
        core.bind(mailbox);
        observability.emit_mailbox_bound(core.actor_id());
        Self {
            core,
            observability,
            rebind_policy,
        }
    }
}

impl<M> Drop for BindingGuard<M> {
    fn drop(&mut self) {
        match self.rebind_policy {
            RebindPolicy::Always | RebindPolicy::OnFailure => self.core.unbind(),
            RebindPolicy::Never => self.core.terminate(),
        }
        self.observability
            .emit_mailbox_cleared(self.core.actor_id());
    }
}
