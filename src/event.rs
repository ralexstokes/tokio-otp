use std::{future::Future, time::Duration};

use tokio::sync::broadcast;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitStatusView {
    Completed,
    Failed(String),
    Panicked,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPathSegment {
    pub id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorEvent {
    SupervisorStarted,
    SupervisorStopping,
    SupervisorStopped,
    Nested {
        id: String,
        generation: u64,
        event: Box<SupervisorEvent>,
    },
    ChildStarted {
        id: String,
        generation: u64,
    },
    ChildExited {
        id: String,
        generation: u64,
        status: ExitStatusView,
    },
    ChildRestartScheduled {
        id: String,
        generation: u64,
        delay: Duration,
    },
    ChildRestarted {
        id: String,
        old_generation: u64,
        new_generation: u64,
    },
    GroupRestartScheduled {
        delay: Duration,
    },
    RestartIntensityExceeded,
}

impl SupervisorEvent {
    pub fn path(&self) -> Vec<EventPathSegment> {
        let mut path = Vec::new();
        self.collect_path(&mut path);
        path
    }

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
