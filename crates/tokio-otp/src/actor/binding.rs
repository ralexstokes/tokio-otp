use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::{mpsc, watch};

use crate::actor::{error::SendError, observability::GraphObservability};

/// A point-in-time snapshot of one actor's message and mailbox statistics.
///
/// Message counters accumulate for the lifetime of the actor binding and
/// therefore survive restarts. Mailbox fields describe the currently bound
/// incarnation and are zero while no mailbox is bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorStats {
    /// Actor id used to correlate these stats with supervisor snapshots.
    pub actor_id: String,
    /// Messages removed from the mailbox by the actor.
    pub messages_received: u64,
    /// Messages accepted by `send` or `try_send`.
    pub messages_accepted: u64,
    /// Total bytes reported for accepted messages when message-size
    /// observation is enabled for this actor.
    ///
    /// `None` means the actor did not opt in. The total accumulates across
    /// actor restarts, like the message counters.
    pub message_bytes_accepted: Option<u64>,
    /// `send` or `try_send` calls that returned an error.
    pub sends_rejected: u64,
    /// Messages currently occupying the bound mailbox.
    pub mailbox_depth: usize,
    /// Maximum capacity of the currently bound mailbox.
    pub mailbox_capacity: usize,
}

#[derive(Debug)]
pub(crate) struct ActorStatsCounters {
    messages_received: AtomicU64,
    messages_accepted: AtomicU64,
    sends_rejected: AtomicU64,
    message_bytes_accepted: Option<AtomicU64>,
}

impl ActorStatsCounters {
    pub(crate) fn new(observe_message_size: bool) -> Self {
        Self {
            messages_received: AtomicU64::new(0),
            messages_accepted: AtomicU64::new(0),
            sends_rejected: AtomicU64::new(0),
            message_bytes_accepted: observe_message_size.then(|| AtomicU64::new(0)),
        }
    }

    pub(crate) fn record_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_send(&self, accepted: bool) {
        let counter = if accepted {
            &self.messages_accepted
        } else {
            &self.sends_rejected
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_message_size(&self, size: usize) {
        if let Some(total) = &self.message_bytes_accepted {
            total.fetch_add(size as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(
        &self,
        actor_id: &str,
        mailbox_depth: usize,
        mailbox_capacity: usize,
    ) -> ActorStats {
        ActorStats {
            actor_id: actor_id.to_owned(),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            messages_accepted: self.messages_accepted.load(Ordering::Relaxed),
            message_bytes_accepted: self
                .message_bytes_accepted
                .as_ref()
                .map(|total| total.load(Ordering::Relaxed)),
            sends_rejected: self.sends_rejected.load(Ordering::Relaxed),
            mailbox_depth,
            mailbox_capacity,
        }
    }
}

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

    pub(crate) fn usage(&self) -> (usize, usize) {
        let capacity = self.sender.max_capacity();
        (capacity.saturating_sub(self.sender.capacity()), capacity)
    }
}

/// The current lifecycle state of an actor mailbox binding.
pub(crate) enum BindingState<M> {
    /// Not yet started, or between restarts where a new mailbox is expected.
    Unbound,
    Bound(MailboxRef<M>),
    /// No restart is scheduled.
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
    fn stats(&self) -> ActorStats;
}

/// Long-lived binding slot for one actor's current mailbox.
///
/// [`ActorRef`](crate::ActorRef)s subscribe to this slot, so they
/// transparently follow the current mailbox across per-actor restarts.
pub(crate) struct BindingCore<M> {
    actor_id: Arc<str>,
    current: watch::Sender<BindingState<M>>,
    stats: Arc<ActorStatsCounters>,
    message_size: Option<fn(&M) -> usize>,
}

impl<M> BindingCore<M> {
    pub(crate) fn new(actor_id: Arc<str>) -> Self {
        Self::with_message_size(actor_id, None)
    }

    pub(crate) fn with_message_size(
        actor_id: Arc<str>,
        message_size: Option<fn(&M) -> usize>,
    ) -> Self {
        let (current, _receiver) = watch::channel(BindingState::Unbound);
        Self {
            actor_id,
            current,
            stats: Arc::new(ActorStatsCounters::new(message_size.is_some())),
            message_size,
        }
    }

    pub(crate) fn actor_id(&self) -> &Arc<str> {
        &self.actor_id
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<BindingState<M>> {
        self.current.subscribe()
    }

    pub(crate) fn stats_counters(&self) -> Arc<ActorStatsCounters> {
        Arc::clone(&self.stats)
    }

    pub(crate) fn message_size(&self) -> Option<fn(&M) -> usize> {
        self.message_size
    }

    pub(crate) fn stats(&self) -> ActorStats {
        let state = self.current.borrow();
        let (depth, capacity) = match &*state {
            BindingState::Bound(mailbox) => mailbox.usage(),
            BindingState::Unbound | BindingState::Terminated => (0, 0),
        };
        self.stats.snapshot(&self.actor_id, depth, capacity)
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

    fn stats(&self) -> ActorStats {
        BindingCore::stats(self)
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
