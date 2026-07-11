use std::sync::{Arc, atomic::AtomicU8};

use tokio::{task::AbortHandle, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    child::ChildDefinition, restart::RestartIntensity, runtime::intensity::RestartTracker,
};

pub(crate) const COMPLETION_PENDING: u8 = 0;
pub(crate) const COMPLETION_CANCELLED: u8 = 1;
pub(crate) const COMPLETION_CLEAN: u8 = 2;

/// Mutable per-child state managed by the supervisor runtime.
///
/// Tracks the child's current lifecycle state, its restart history, and the
/// handles needed to cancel or abort the running Tokio task.
pub(crate) struct ChildRuntime {
    pub(crate) definition: Arc<ChildDefinition>,
    pub(crate) restart_tracker: RestartTracker,
    pub(crate) generation: u64,
    pub(crate) state: RuntimeChildState,
    pub(crate) active_token: Option<CancellationToken>,
    pub(crate) abort_handle: Option<AbortHandle>,
    pub(crate) has_started: bool,
    pub(crate) has_reported_ready: bool,
    pub(crate) startup_aborted: bool,
    pub(crate) next_restart_deadline: Option<Instant>,
    /// Atomically orders a natural clean return against supervisor-driven
    /// cancellation.
    pub(crate) completion_state: Arc<AtomicU8>,
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
        definition: Arc<ChildDefinition>,
        default_restart_intensity: RestartIntensity,
    ) -> Self {
        let restart_intensity = definition
            .restart_intensity
            .unwrap_or(default_restart_intensity);
        Self {
            definition,
            restart_tracker: RestartTracker::new(restart_intensity),
            generation: 0,
            state: RuntimeChildState::Stopped,
            active_token: None,
            abort_handle: None,
            has_started: false,
            has_reported_ready: false,
            startup_aborted: false,
            next_restart_deadline: None,
            completion_state: Arc::new(AtomicU8::new(COMPLETION_PENDING)),
        }
    }
}
