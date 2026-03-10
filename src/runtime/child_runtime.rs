use std::sync::Arc;

use tokio::{task::AbortHandle, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{child::ChildSpecInner, restart::RestartIntensity, runtime::intensity::RestartTracker};

pub(crate) struct ChildRuntime {
    pub(crate) spec: Arc<ChildSpecInner>,
    pub(crate) restart_tracker: RestartTracker,
    pub(crate) generation: u64,
    pub(crate) state: RuntimeChildState,
    pub(crate) active_token: Option<CancellationToken>,
    pub(crate) abort_handle: Option<AbortHandle>,
    pub(crate) has_started: bool,
    pub(crate) next_restart_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeChildState {
    Starting,
    Running,
    Stopping,
    Stopped,
}

impl RuntimeChildState {
    pub(crate) fn is_active(self) -> bool {
        !matches!(self, Self::Stopped)
    }
}

impl ChildRuntime {
    pub(crate) fn new(
        spec: Arc<ChildSpecInner>,
        default_restart_intensity: RestartIntensity,
    ) -> Self {
        let restart_intensity = spec.restart_intensity.unwrap_or(default_restart_intensity);
        Self {
            spec,
            restart_tracker: RestartTracker::new(restart_intensity),
            generation: 0,
            state: RuntimeChildState::Stopped,
            active_token: None,
            abort_handle: None,
            has_started: false,
            next_restart_deadline: None,
        }
    }
}
