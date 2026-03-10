use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::sync::mpsc;

use crate::{error::SupervisorExit, event::ExitStatusView, strategy::Strategy};

/// Point-in-time snapshot of a supervisor's state, including the state of every
/// child.
///
/// Snapshots are published via a `tokio::sync::watch` channel. The supervisor
/// updates the snapshot **before** broadcasting the corresponding
/// [`SupervisorEvent`](crate::SupervisorEvent), so subscribers reading the
/// snapshot from an event handler will see already-consistent state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorSnapshot {
    /// Current lifecycle state of the supervisor.
    pub state: SupervisorStateView,
    /// The supervisor's last exit reason, if it has stopped.
    pub last_exit: Option<SupervisorExit>,
    /// The restart strategy in use.
    pub strategy: Strategy,
    /// Ordered list of child snapshots, matching the supervisor's child order.
    pub children: Vec<ChildSnapshot>,
}

/// Point-in-time snapshot of a single child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildSnapshot {
    /// The child's unique identifier.
    pub id: String,
    /// Current generation counter. Incremented on each restart.
    pub generation: u64,
    /// Current lifecycle state.
    pub state: ChildStateView,
    /// Whether the child is active or being removed.
    pub membership: ChildMembershipView,
    /// How the child last exited, if it has exited at least once.
    pub last_exit: Option<ExitStatusView>,
    /// Total number of times this child has been restarted.
    pub restart_count: u64,
    /// Time remaining until the next scheduled restart, if a backoff delay is
    /// pending.
    pub next_restart_in: Option<Duration>,
    /// If this child is itself a nested supervisor (created via
    /// [`Supervisor::into_child_spec`](crate::Supervisor::into_child_spec)),
    /// this contains its recursive snapshot.
    pub supervisor: Option<Box<SupervisorSnapshot>>,
}

impl SupervisorSnapshot {
    /// Looks up a direct child by id.
    pub fn child(&self, id: &str) -> Option<&ChildSnapshot> {
        self.children.iter().find(|child| child.id == id)
    }

    /// Walks a dot-separated path of child ids through nested supervisors.
    ///
    /// For example, `descendant(["db_pool", "writer"])` first finds child
    /// `"db_pool"` at this level, then looks for `"writer"` inside its nested
    /// supervisor snapshot.
    pub fn descendant<I, S>(&self, path: I) -> Option<&ChildSnapshot>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut path = path.into_iter();
        let mut child = self.child(path.next()?.as_ref())?;

        for segment in path {
            child = child.child(segment.as_ref())?;
        }

        Some(child)
    }
}

impl ChildSnapshot {
    /// Looks up a grandchild by id within this child's nested supervisor
    /// snapshot. Returns `None` if this child is not a nested supervisor or
    /// has no child with the given id.
    pub fn child(&self, id: &str) -> Option<&ChildSnapshot> {
        self.supervisor.as_deref()?.child(id)
    }

    /// Walks a path through nested supervisor snapshots starting from this
    /// child. See [`SupervisorSnapshot::descendant`] for details.
    pub fn descendant<I, S>(&self, path: I) -> Option<&ChildSnapshot>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.as_deref()?.descendant(path)
    }
}

/// Lifecycle state of a supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorStateView {
    /// The supervisor is running and accepting commands.
    Running,
    /// The supervisor is shutting down (children are being stopped).
    Stopping,
    /// The supervisor has fully stopped.
    Stopped,
}

/// Lifecycle state of a child task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStateView {
    /// The child has been created but its task has not yet started running.
    Starting,
    /// The child task is running.
    Running,
    /// The child is in the process of being stopped (token cancelled, waiting
    /// for exit).
    Stopping,
    /// The child has exited.
    Stopped,
}

/// Whether a child is a permanent member of the supervisor or is being removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildMembershipView {
    /// The child is an active member of the supervisor.
    Active,
    /// A removal has been requested and is in progress.
    Removing,
}

// ---------------------------------------------------------------------------
// Internal: nested snapshot forwarding
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct NestedSnapshotNotification {
    pub(crate) parent_key: usize,
    pub(crate) parent_instance: u64,
    pub(crate) generation: u64,
}

/// Coalescing state for nested supervisor snapshot updates.
///
/// Uses an atomic `queued` flag to avoid flooding the parent's notification
/// channel: if a notification is already in flight, subsequent updates replace
/// the stored snapshot but do not send another notification. The parent
/// dequeues the latest snapshot when it processes the notification.
#[derive(Clone, Default)]
pub(crate) struct NestedSnapshotState {
    latest: Arc<Mutex<Option<SupervisorSnapshot>>>,
    queued: Arc<AtomicBool>,
}

impl NestedSnapshotState {
    pub(crate) fn clear(&self) {
        *self
            .latest
            .lock()
            .expect("nested snapshot state mutex poisoned") = None;
        self.queued.store(false, Ordering::Release);
    }

    pub(crate) fn replace(&self, snapshot: SupervisorSnapshot) {
        *self
            .latest
            .lock()
            .expect("nested snapshot state mutex poisoned") = Some(snapshot);
    }

    pub(crate) fn latest(&self) -> Option<SupervisorSnapshot> {
        self.latest
            .lock()
            .expect("nested snapshot state mutex poisoned")
            .clone()
    }

    /// Attempts to mark a notification as queued. Returns `true` if this call
    /// transitioned the flag from `false` to `true` (i.e. the caller should
    /// send a notification). Returns `false` if a notification was already
    /// queued.
    pub(crate) fn try_queue(&self) -> bool {
        !self.queued.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn mark_dequeued(&self) {
        self.queued.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct NestedSnapshotForwarder {
    notifications: mpsc::Sender<NestedSnapshotNotification>,
    state: NestedSnapshotState,
    parent_key: usize,
    parent_instance: u64,
    generation: u64,
}

impl NestedSnapshotForwarder {
    pub(crate) fn new(
        notifications: mpsc::Sender<NestedSnapshotNotification>,
        state: NestedSnapshotState,
        parent_key: usize,
        parent_instance: u64,
        generation: u64,
    ) -> Self {
        Self {
            notifications,
            state,
            parent_key,
            parent_instance,
            generation,
        }
    }

    fn forward(&self, snapshot: SupervisorSnapshot) {
        self.state.replace(snapshot);
        if !self.state.try_queue() {
            return;
        }

        let notification = NestedSnapshotNotification {
            parent_key: self.parent_key,
            parent_instance: self.parent_instance,
            generation: self.generation,
        };

        match self.notifications.try_send(notification) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(notification)) => {
                let notifications = self.notifications.clone();
                let state = self.state.clone();
                tokio::spawn(async move {
                    if notifications.send(notification).await.is_err() {
                        state.mark_dequeued();
                    }
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.state.mark_dequeued();
            }
        }
    }
}

tokio::task_local! {
    static NESTED_SNAPSHOT_FORWARDER: NestedSnapshotForwarder;
}

pub(crate) async fn with_nested_snapshot_forwarder<Fut>(
    forwarder: NestedSnapshotForwarder,
    future: Fut,
) -> Fut::Output
where
    Fut: Future,
{
    NESTED_SNAPSHOT_FORWARDER.scope(forwarder, future).await
}

pub(crate) fn forward_nested_snapshot(snapshot: SupervisorSnapshot) {
    let _ = NESTED_SNAPSHOT_FORWARDER.try_with(|forwarder| {
        forwarder.forward(snapshot);
    });
}
