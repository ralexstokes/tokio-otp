use std::time::Duration;

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
