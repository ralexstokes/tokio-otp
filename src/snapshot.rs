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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorSnapshot {
    pub state: SupervisorStateView,
    pub last_exit: Option<SupervisorExit>,
    pub strategy: Strategy,
    pub children: Vec<ChildSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildSnapshot {
    pub id: String,
    pub generation: u64,
    pub state: ChildStateView,
    pub membership: ChildMembershipView,
    pub last_exit: Option<ExitStatusView>,
    pub restart_count: u64,
    pub next_restart_in: Option<Duration>,
    pub supervisor: Option<Box<SupervisorSnapshot>>,
}

impl SupervisorSnapshot {
    pub fn child(&self, id: &str) -> Option<&ChildSnapshot> {
        self.children.iter().find(|child| child.id == id)
    }

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
    pub fn child(&self, id: &str) -> Option<&ChildSnapshot> {
        self.supervisor.as_deref()?.child(id)
    }

    pub fn descendant<I, S>(&self, path: I) -> Option<&ChildSnapshot>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.as_deref()?.descendant(path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorStateView {
    Running,
    Stopping,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStateView {
    Starting,
    Running,
    Stopping,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildMembershipView {
    Active,
    Removing,
}

#[derive(Clone, Debug)]
pub(crate) struct NestedSnapshotNotification {
    pub(crate) parent_key: usize,
    pub(crate) parent_instance: u64,
    pub(crate) generation: u64,
}

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
