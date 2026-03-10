use std::{io::Error, sync::Arc};

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{Instrument, info_span};

use crate::{
    child::{ChildResult, ChildSpec, ChildSpecInner},
    context::ChildContext,
    error::{SupervisorError, SupervisorExit},
    event::forward_nested_event,
    handle::{
        NestedControlRegistry, NestedControlScope, SupervisorCommand, SupervisorHandle,
        SupervisorHandleInit, current_nested_control_scope,
    },
    observability::{
        SupervisorObservability, format_path, strategy_label, supervisor_name_for_path,
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
    pub(crate) control_channel_capacity: usize,
    pub(crate) event_channel_capacity: usize,
}

impl Supervisor {
    pub(crate) fn new(config: SupervisorConfig) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<SupervisorExit, SupervisorError> {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (events_tx, _) = broadcast::channel(self.config.event_channel_capacity);
        let (_command_tx, command_rx) = mpsc::channel(self.config.control_channel_capacity);
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
        let (command_tx, command_rx) = mpsc::channel(self.config.control_channel_capacity);
        let (done_tx, done_rx) = watch::channel(None);
        let (events_tx, _) = broadcast::channel(self.config.event_channel_capacity);
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
        command_rx: mpsc::Receiver<SupervisorCommand>,
        registry: Arc<NestedControlRegistry>,
        path_prefix: Vec<String>,
    ) -> Result<SupervisorExit, SupervisorError> {
        let supervisor_name = supervisor_name_for_path(&path_prefix).to_owned();
        let supervisor_path = format_path(&path_prefix);
        let strategy = strategy_label(self.config.strategy);
        let mut runtime = SupervisorRuntime::new(
            self.config,
            shutdown_rx,
            events_tx,
            snapshots_tx,
            command_rx,
            registry,
            path_prefix,
        );
        runtime
            .run()
            .instrument(info_span!(
                "supervisor",
                supervisor_name = %supervisor_name,
                supervisor_path = %supervisor_path,
                strategy,
            ))
            .await
    }

    async fn run_as_child(self, ctx: ChildContext) -> ChildResult {
        let child_id = ctx.id.clone();
        let control_scope = current_nested_control_scope().unwrap_or_else(|| {
            NestedControlScope::new(
                Arc::new(NestedControlRegistry::default()),
                vec![child_id.clone()],
            )
        });
        let child_path = control_scope.child_path();
        let handle = self.spawn_with_control(control_scope.registry(), child_path.clone());
        let _registration = control_scope.register(handle.control_endpoint());
        let mut events_rx = handle.subscribe();
        let mut snapshots_rx = handle.subscribe_snapshots();
        let initial_snapshot = handle.snapshot();
        let observability = SupervisorObservability::new(child_path, initial_snapshot.strategy);
        forward_nested_snapshot(initial_snapshot);
        let wait = handle.wait();
        tokio::pin!(wait);
        let mut shutdown_requested = false;

        loop {
            tokio::select! {
                biased;
                result = &mut wait => {
                    drain_nested_events(&mut events_rx);
                    return map_nested_supervisor_result(&child_id, result);
                }
                maybe_event = events_rx.recv() => {
                    handle_nested_event(&observability, maybe_event);
                }
                changed = snapshots_rx.changed() => {
                    forward_nested_snapshot_change(&snapshots_rx, changed);
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

fn map_nested_supervisor_result(
    child_id: &str,
    result: Result<SupervisorExit, SupervisorError>,
) -> ChildResult {
    match result {
        Ok(SupervisorExit::Completed | SupervisorExit::Shutdown) => Ok(()),
        Ok(SupervisorExit::Failed) => Err(Box::new(Error::other(format!(
            "nested supervisor `{child_id}` failed"
        )))),
        Err(err) => Err(Box::new(err)),
    }
}

fn handle_nested_event(
    observability: &SupervisorObservability,
    maybe_event: Result<crate::event::SupervisorEvent, broadcast::error::RecvError>,
) {
    match maybe_event {
        Ok(event) => forward_nested_event(event),
        Err(broadcast::error::RecvError::Lagged(dropped)) => {
            observability.emit_nested_event_forwarding_lag(dropped);
        }
        Err(broadcast::error::RecvError::Closed) => {}
    }
}

fn forward_nested_snapshot_change(
    snapshots_rx: &watch::Receiver<SupervisorSnapshot>,
    changed: Result<(), watch::error::RecvError>,
) {
    if changed.is_ok() {
        forward_nested_snapshot(snapshots_rx.borrow().clone());
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
                state: ChildStateView::Starting,
                membership: ChildMembershipView::Active,
                last_exit: None,
                restart_count: 0,
                next_restart_in: None,
                supervisor: None,
            })
            .collect(),
    }
}
