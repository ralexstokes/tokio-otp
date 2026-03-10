use std::{io::Error, sync::Arc};

use tokio::sync::{broadcast, watch};

use crate::{
    child::{ChildResult, ChildSpec, ChildSpecInner},
    context::ChildContext,
    error::{SupervisorError, SupervisorExit},
    event::forward_nested_event,
    handle::SupervisorHandle,
    restart::RestartIntensity,
    runtime::SupervisorRuntime,
    strategy::Strategy,
};

#[derive(Clone)]
pub struct Supervisor {
    pub(crate) config: SupervisorConfig,
}

#[derive(Clone)]
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
        self.run_with_channels(shutdown_rx, events_tx).await
    }

    pub fn spawn(self) -> SupervisorHandle {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(None);
        let (events_tx, _) = broadcast::channel(256);
        let task_done_tx = done_tx.clone();
        let task_events_tx = events_tx.clone();

        let join_handle = tokio::spawn(async move {
            let result = self.run_with_channels(shutdown_rx, task_events_tx).await;
            let _ = task_done_tx.send(Some(result.clone()));
            result
        });

        SupervisorHandle::new(shutdown_tx, done_tx, done_rx, events_tx, join_handle)
    }

    /// Adapts this supervisor into a restartable child of another supervisor.
    ///
    /// The returned child forwards parent cancellation into a graceful shutdown of
    /// the nested supervisor. Apply outer restart and shutdown policies to the
    /// returned [`ChildSpec`] as needed.
    pub fn into_child_spec(self, id: impl Into<String>) -> ChildSpec {
        let supervisor = self;
        ChildSpec::new(id, move |ctx| supervisor.clone().run_as_child(ctx))
    }

    async fn run_with_channels(
        self,
        shutdown_rx: watch::Receiver<bool>,
        events_tx: broadcast::Sender<crate::event::SupervisorEvent>,
    ) -> Result<SupervisorExit, SupervisorError> {
        let mut runtime = SupervisorRuntime::new(self.config, shutdown_rx, events_tx);
        runtime.run().await
    }

    async fn run_as_child(self, ctx: ChildContext) -> ChildResult {
        let child_id = ctx.id.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (events_tx, mut events_rx) = broadcast::channel(256);
        let run = self.run_with_channels(shutdown_rx, events_tx);
        tokio::pin!(run);
        let mut shutdown_requested = false;

        loop {
            tokio::select! {
                biased;
                result = &mut run => {
                    drain_nested_events(&mut events_rx);
                    return match result {
                        Ok(SupervisorExit::Completed | SupervisorExit::Shutdown) => Ok(()),
                        Ok(SupervisorExit::Failed) => Err(Box::new(Error::other(format!(
                            "nested supervisor `{child_id}` failed"
                        )))),
                        Err(err) => Err(Box::new(err)),
                    };
                }
                maybe_event = events_rx.recv() => {
                    match maybe_event {
                        Ok(event) => forward_nested_event(event),
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {}
                    }
                }
                _ = ctx.token.cancelled(), if !shutdown_requested => {
                    shutdown_requested = true;
                    let _ = shutdown_tx.send(true);
                }
            }
        }
    }
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}

fn drain_nested_events(events_rx: &mut broadcast::Receiver<crate::event::SupervisorEvent>) {
    loop {
        match events_rx.try_recv() {
            Ok(event) => forward_nested_event(event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => break,
        }
    }
}
