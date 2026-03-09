use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};

use crate::{
    child::ChildSpecInner,
    error::{SupervisorError, SupervisorExit},
    handle::SupervisorHandle,
    restart::RestartIntensity,
    runtime::SupervisorRuntime,
    strategy::Strategy,
};

pub struct Supervisor {
    pub(crate) config: SupervisorConfig,
}

pub(crate) struct SupervisorConfig {
    pub(crate) strategy: Strategy,
    pub(crate) restart_intensity: RestartIntensity,
    pub(crate) children: Vec<Arc<ChildSpecInner>>,
}

impl Supervisor {
    pub(crate) fn new(config: SupervisorConfig) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<SupervisorExit, SupervisorError> {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (events_tx, _) = broadcast::channel(256);
        let mut runtime = SupervisorRuntime::new(self.config, shutdown_rx, events_tx);
        runtime.run().await
    }

    pub fn spawn(self) -> SupervisorHandle {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(None);
        let (events_tx, _) = broadcast::channel(256);
        let task_done_tx = done_tx.clone();
        let task_events_tx = events_tx.clone();

        let join_handle = tokio::spawn(async move {
            let mut runtime = SupervisorRuntime::new(self.config, shutdown_rx, task_events_tx);
            let result = runtime.run().await;
            let _ = task_done_tx.send(Some(result.clone()));
            result
        });

        SupervisorHandle {
            shutdown_tx,
            done_rx,
            done_tx,
            events_tx,
            join_handle: Arc::new(Mutex::new(Some(join_handle))),
        }
    }
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}
