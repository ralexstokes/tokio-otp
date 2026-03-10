use std::{future::Future, time::Duration};

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
pub(crate) struct NestedSnapshotUpdate {
    pub(crate) parent_id: String,
    pub(crate) generation: u64,
    pub(crate) snapshot: SupervisorSnapshot,
}

#[derive(Clone)]
pub(crate) struct NestedSnapshotForwarder {
    updates: mpsc::UnboundedSender<NestedSnapshotUpdate>,
    parent_id: String,
    generation: u64,
}

impl NestedSnapshotForwarder {
    pub(crate) fn new(
        updates: mpsc::UnboundedSender<NestedSnapshotUpdate>,
        parent_id: String,
        generation: u64,
    ) -> Self {
        Self {
            updates,
            parent_id,
            generation,
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
        let _ = forwarder.updates.send(NestedSnapshotUpdate {
            parent_id: forwarder.parent_id.clone(),
            generation: forwarder.generation,
            snapshot,
        });
    });
}
