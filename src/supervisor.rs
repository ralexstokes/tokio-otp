use std::{io::Error, sync::Arc};

use tokio::sync::{broadcast, mpsc, watch};

use crate::{
    child::{ChildResult, ChildSpec, ChildSpecInner},
    context::ChildContext,
    error::{SupervisorError, SupervisorExit},
    event::forward_nested_event,
    handle::{
        NestedControlRegistry, NestedControlScope, SupervisorCommand, SupervisorHandle,
        SupervisorHandleInit, current_nested_control_scope,
    },
    restart::RestartIntensity,
    runtime::SupervisorRuntime,
    snapshot::{
        ChildMembershipView, ChildSnapshot, ChildStateView, SupervisorSnapshot,
        SupervisorStateView, forward_nested_snapshot,
    },
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
        let (_command_tx, command_rx) = mpsc::unbounded_channel();
        let (snapshots_tx, _) = watch::channel(initial_snapshot(&self.config));
        self.run_with_channels(
            shutdown_rx,
            events_tx,
            snapshots_tx,
            command_rx,
            Arc::new(NestedControlRegistry::default()),
            Vec::new(),
        )
        .await
    }

    pub fn spawn(self) -> SupervisorHandle {
        self.spawn_with_control(Arc::new(NestedControlRegistry::default()), Vec::new())
    }

    fn spawn_with_control(
        self,
        registry: Arc<NestedControlRegistry>,
        path_prefix: Vec<String>,
    ) -> SupervisorHandle {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = watch::channel(None);
        let (events_tx, _) = broadcast::channel(256);
        let (snapshots_tx, snapshots_rx) = watch::channel(initial_snapshot(&self.config));
        let task_done_tx = done_tx.clone();
        let task_events_tx = events_tx.clone();
        let task_snapshots_tx = snapshots_tx.clone();
        let task_registry = Arc::clone(&registry);
        let task_path_prefix = path_prefix.clone();

        let join_handle = tokio::spawn(async move {
            let result = self
                .run_with_channels(
                    shutdown_rx,
                    task_events_tx,
                    task_snapshots_tx,
                    command_rx,
                    task_registry,
                    task_path_prefix,
                )
                .await;
            let _ = task_done_tx.send(Some(result.clone()));
            result
        });

        SupervisorHandle::new(SupervisorHandleInit {
            shutdown_tx,
            command_tx,
            registry,
            path_prefix,
            done_tx,
            done_rx,
            events_tx,
            snapshots_rx,
            join_handle,
        })
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
        snapshots_tx: watch::Sender<SupervisorSnapshot>,
        command_rx: mpsc::UnboundedReceiver<SupervisorCommand>,
        registry: Arc<NestedControlRegistry>,
        path_prefix: Vec<String>,
    ) -> Result<SupervisorExit, SupervisorError> {
        let mut runtime = SupervisorRuntime::new(
            self.config,
            shutdown_rx,
            events_tx,
            snapshots_tx,
            command_rx,
            registry,
            path_prefix,
        );
        runtime.run().await
    }

    async fn run_as_child(self, ctx: ChildContext) -> ChildResult {
        let child_id = ctx.id.clone();
        let control_scope = current_nested_control_scope().unwrap_or_else(|| {
            NestedControlScope::new(
                Arc::new(NestedControlRegistry::default()),
                vec![child_id.clone()],
            )
        });
        let handle = self.spawn_with_control(control_scope.registry(), control_scope.child_path());
        let _registration = control_scope.register(handle.control_endpoint());
        let mut events_rx = handle.subscribe();
        let mut snapshots_rx = handle.subscribe_snapshots();
        forward_nested_snapshot(handle.snapshot());
        let wait = handle.wait();
        tokio::pin!(wait);
        let mut shutdown_requested = false;

        loop {
            tokio::select! {
                biased;
                result = &mut wait => {
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
                changed = snapshots_rx.changed() => {
                    if let Ok(()) = changed {
                        forward_nested_snapshot(snapshots_rx.borrow().clone());
                    }
                }
                _ = ctx.token.cancelled(), if !shutdown_requested => {
                    shutdown_requested = true;
                    handle.shutdown();
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

fn initial_snapshot(config: &SupervisorConfig) -> SupervisorSnapshot {
    SupervisorSnapshot {
        state: SupervisorStateView::Running,
        last_exit: None,
        strategy: config.strategy,
        children: config
            .children
            .iter()
            .map(|spec| ChildSnapshot {
                id: spec.id.clone(),
                generation: 0,
                state: ChildStateView::Stopped,
                membership: ChildMembershipView::Active,
                last_exit: None,
                restart_count: 0,
                next_restart_in: None,
                supervisor: None,
            })
            .collect(),
    }
}
