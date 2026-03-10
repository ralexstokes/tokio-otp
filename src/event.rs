use std::{future::Future, time::Duration};

use tokio::sync::broadcast;

/// Snapshot of how a child task exited.
///
/// This is a cloneable, displayable view of the exit status; the original error
/// value (if any) is converted to its `Display` string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitStatusView {
    /// The child returned `Ok(())`.
    Completed,
    /// The child returned an `Err`. The string is the error's `Display` output.
    Failed(String),
    /// The child task panicked.
    Panicked,
    /// The child task was aborted by the supervisor (e.g. after a grace-period
    /// timeout).
    Aborted,
}

/// One segment of a [`SupervisorEvent::Nested`] path, identifying which child
/// supervisor forwarded the event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPathSegment {
    /// The child id of the nested supervisor that forwarded this event.
    pub id: String,
    /// The generation of that child at the time the event was forwarded.
    pub generation: u64,
}

/// Lifecycle event emitted by a supervisor.
///
/// Events are broadcast to all subscribers via
/// [`SupervisorHandle::subscribe`](crate::SupervisorHandle::subscribe).
///
/// # Ordering guarantee
///
/// The supervisor publishes an updated [`SupervisorSnapshot`](crate::SupervisorSnapshot)
/// **before** broadcasting the corresponding event, so event handlers can read
/// already-consistent snapshot state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorEvent {
    /// The supervisor has started and all initial children are being spawned.
    SupervisorStarted,
    /// The supervisor is beginning its shutdown sequence.
    SupervisorStopping,
    /// The supervisor has fully stopped. No further events will be emitted.
    ///
    /// Emitted after explicit shutdown and after natural completion or
    /// failure once all child tasks have been joined.
    SupervisorStopped,
    /// An event forwarded from a nested (child) supervisor. Nested events form
    /// a recursive wrapper; use [`path`](SupervisorEvent::path) or
    /// [`leaf`](SupervisorEvent::leaf) to unwrap.
    Nested {
        /// Child id of the nested supervisor.
        id: String,
        /// Generation of that child.
        generation: u64,
        /// The inner event from the nested supervisor.
        event: Box<SupervisorEvent>,
    },
    /// A child task has been spawned and is now running.
    ChildStarted {
        /// Child identifier.
        id: String,
        /// Generation counter for this spawn.
        generation: u64,
    },
    /// A runtime removal of this child has been requested.
    ChildRemoveRequested {
        /// Child identifier.
        id: String,
    },
    /// The child has been fully removed from the supervisor.
    ChildRemoved {
        /// Child identifier.
        id: String,
    },
    /// A child task exited (cleanly, with an error, by panic, or by abort).
    ChildExited {
        /// Child identifier.
        id: String,
        /// Generation that exited.
        generation: u64,
        /// How the child exited.
        status: ExitStatusView,
    },
    /// A restart for this child has been scheduled after a backoff delay.
    ChildRestartScheduled {
        /// Child identifier.
        id: String,
        /// Generation that exited and will be replaced.
        generation: u64,
        /// How long the supervisor will wait before respawning.
        delay: Duration,
    },
    /// A child has been successfully restarted with a new generation.
    ChildRestarted {
        /// Child identifier.
        id: String,
        /// Generation that exited.
        old_generation: u64,
        /// Generation of the newly spawned replacement.
        new_generation: u64,
    },
    /// A [`OneForAll`](crate::Strategy::OneForAll) group restart has been
    /// scheduled after a backoff delay.
    GroupRestartScheduled {
        /// How long the supervisor will wait before restarting all children.
        delay: Duration,
    },
    /// The restart intensity limit was exceeded. The supervisor will exit with
    /// [`SupervisorError::RestartIntensityExceeded`](crate::SupervisorError::RestartIntensityExceeded).
    RestartIntensityExceeded,
}

impl SupervisorEvent {
    /// Returns the nested-supervisor path leading to this event.
    ///
    /// For non-nested events the path is empty. For events wrapped in one or
    /// more [`Nested`](SupervisorEvent::Nested) layers, each layer contributes
    /// one [`EventPathSegment`].
    pub fn path(&self) -> Vec<EventPathSegment> {
        let mut path = Vec::new();
        self.collect_path(&mut path);
        path
    }

    /// Unwraps any [`Nested`](SupervisorEvent::Nested) wrappers and returns
    /// the innermost (leaf) event.
    pub fn leaf(&self) -> &Self {
        match self {
            Self::Nested { event, .. } => event.leaf(),
            event => event,
        }
    }

    fn collect_path(&self, path: &mut Vec<EventPathSegment>) {
        if let Self::Nested {
            id,
            generation,
            event,
        } = self
        {
            path.push(EventPathSegment {
                id: id.clone(),
                generation: *generation,
            });
            event.collect_path(path);
        }
    }
}

#[derive(Clone)]
pub(crate) struct NestedEventForwarder {
    events: broadcast::Sender<SupervisorEvent>,
    parent_id: String,
    generation: u64,
}

impl NestedEventForwarder {
    pub(crate) fn new(
        events: broadcast::Sender<SupervisorEvent>,
        parent_id: String,
        generation: u64,
    ) -> Self {
        Self {
            events,
            parent_id,
            generation,
        }
    }
}

tokio::task_local! {
    static NESTED_EVENT_FORWARDER: NestedEventForwarder;
}

pub(crate) async fn with_nested_event_forwarder<Fut>(
    forwarder: NestedEventForwarder,
    future: Fut,
) -> Fut::Output
where
    Fut: Future,
{
    NESTED_EVENT_FORWARDER.scope(forwarder, future).await
}

pub(crate) fn forward_nested_event(event: SupervisorEvent) {
    let _ = NESTED_EVENT_FORWARDER.try_with(|forwarder| {
        let _ = forwarder.events.send(SupervisorEvent::Nested {
            id: forwarder.parent_id.clone(),
            generation: forwarder.generation,
            event: Box::new(event),
        });
    });
}
