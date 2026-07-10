use std::time::Duration;

use tokio::sync::mpsc;

use crate::observability::SupervisorObservability;

/// Snapshot of how a child task exited.
///
/// This is a cloneable, displayable view of the exit status; the original error
/// value (if any) is converted to its `Display` string.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SupervisorEvent {
    /// The supervisor has started and all initial children are being spawned.
    SupervisorStarted,
    /// The supervisor is beginning its shutdown sequence.
    SupervisorStopping,
    /// The supervisor has fully stopped. No further events will be emitted.
    ///
    /// Emitted after explicit shutdown or a fatal supervisor error once all
    /// child tasks have been joined.
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

pub(crate) struct NestedEventNotification {
    pub(crate) parent_key: usize,
    pub(crate) parent_instance: u64,
    pub(crate) id: String,
    pub(crate) generation: u64,
    pub(crate) event: SupervisorEvent,
}

#[derive(Clone)]
pub(crate) struct EventSink {
    notifications: mpsc::Sender<NestedEventNotification>,
    parent_key: usize,
    parent_instance: u64,
    observability: SupervisorObservability,
}

impl EventSink {
    pub(crate) fn new(
        notifications: mpsc::Sender<NestedEventNotification>,
        parent_key: usize,
        parent_instance: u64,
        observability: SupervisorObservability,
    ) -> Self {
        Self {
            notifications,
            parent_key,
            parent_instance,
            observability,
        }
    }

    pub(crate) fn forward(&self, id: String, generation: u64, event: SupervisorEvent) {
        let notification = NestedEventNotification {
            parent_key: self.parent_key,
            parent_instance: self.parent_instance,
            id,
            generation,
            event,
        };
        if let Err(error) = self.notifications.try_send(notification)
            && matches!(error, mpsc::error::TrySendError::Full(_))
        {
            self.observability.emit_nested_event_forwarding_lag(1);
        }
    }
}
