use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitStatusView {
    Completed,
    Failed(String),
    Panicked,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorEvent {
    SupervisorStarted,
    SupervisorStopping,
    SupervisorStopped,
    ChildStarted {
        id: Arc<str>,
        generation: u64,
    },
    ChildExited {
        id: Arc<str>,
        generation: u64,
        status: ExitStatusView,
    },
    ChildRestartScheduled {
        id: Arc<str>,
        generation: u64,
        delay: Duration,
    },
    ChildRestarted {
        id: Arc<str>,
        old_generation: u64,
        new_generation: u64,
    },
    GroupRestartScheduled {
        delay: Duration,
    },
    RestartIntensityExceeded,
}
