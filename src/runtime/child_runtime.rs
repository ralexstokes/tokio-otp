use std::sync::Arc;

use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::child::ChildSpecInner;

pub(crate) struct ChildRuntime {
    pub(crate) spec: Arc<ChildSpecInner>,
    pub(crate) generation: u64,
    pub(crate) state: RuntimeChildState,
    pub(crate) active_token: Option<CancellationToken>,
    pub(crate) abort_handle: Option<AbortHandle>,
    pub(crate) has_started: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeChildState {
    Starting,
    Running,
    Stopping,
    Stopped,
}

impl ChildRuntime {
    pub(crate) fn new(spec: Arc<ChildSpecInner>) -> Self {
        Self {
            spec,
            generation: 0,
            state: RuntimeChildState::Stopped,
            active_token: None,
            abort_handle: None,
            has_started: false,
        }
    }
}
