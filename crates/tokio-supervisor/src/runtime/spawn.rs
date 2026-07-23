use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use crate::{
    child::{ChildKind, ChildReadiness},
    context::{ChildContext, ChildReady, ReadySignal, SupervisorToken},
    error::SupervisorError,
    event::{EventSink, SupervisorEvent},
    runtime::{
        child_runtime::{COMPLETION_CLEAN, COMPLETION_PENDING, RuntimeChildState},
        supervision::{ChildEnvelope, SupervisorRuntime, TaskMeta},
    },
    snapshot::{NestedSnapshotState, SnapshotCell},
    supervisor::ParentLink,
};
use tracing::{Instrument, info_span};

impl SupervisorRuntime {
    pub(crate) fn spawn_child(
        &mut self,
        key: usize,
    ) -> Result<(Option<u64>, u64), SupervisorError> {
        let (
            child_id,
            generation,
            old_generation,
            kind,
            ctx,
            counted_before,
            instance,
            snapshot_state,
            nested_channels,
            completion_state,
        ) = {
            let entry = self.children.get_mut(key).ok_or_else(|| {
                SupervisorError::Internal(format!("missing child slot for key {key}"))
            })?;
            let child = &mut entry.runtime;
            let counted_before = matches!(
                child.state,
                RuntimeChildState::Starting | RuntimeChildState::Running
            );
            let instance = entry.instance;

            let old_generation = if child.has_started {
                Some(child.generation)
            } else {
                None
            };
            if child.has_started {
                child.generation = child.generation.saturating_add(1);
            }

            let generation = child.generation;
            child
                .restart_tracker
                .record_spawn(tokio::time::Instant::now());
            let child_token = self.group_token.child_token();
            child.active_token = Some(child_token.clone());
            child.state = RuntimeChildState::Starting;
            child.has_reported_ready = false;
            child.startup_aborted = false;
            child.next_restart_deadline = None;
            let completion_state = Arc::new(AtomicU8::new(COMPLETION_PENDING));
            child.completion_state = completion_state.clone();
            entry.nested_snapshot = None;
            let snapshot_state = if matches!(&child.definition.kind, ChildKind::Supervisor(_)) {
                let state = NestedSnapshotState::default();
                entry.nested_snapshot_state = Some(state.clone());
                Some(state)
            } else {
                entry.nested_snapshot_state = None;
                None
            };

            let child_id = entry.id.clone();
            let ctx = ChildContext::new(
                child_id.clone(),
                generation,
                child_token,
                SupervisorToken::new(self.group_token.clone()),
                (child.definition.readiness == ChildReadiness::Explicit).then(|| {
                    ReadySignal::new(
                        self.ready_tx.clone(),
                        ChildReady {
                            key,
                            instance,
                            generation,
                        },
                    )
                }),
            );
            let kind = child.definition.kind.clone();
            let nested_channels = entry.nested_channels.clone();

            (
                child_id,
                generation,
                old_generation,
                kind,
                ctx,
                counted_before,
                instance,
                snapshot_state,
                nested_channels,
                completion_state,
            )
        };
        let child_path_segments = self.child_path(key);
        // A nested supervisor can be revived after this incarnation ends if
        // its own policy permits a restart, or if this supervisor can itself
        // be reincarnated (which respawns every static child). Its
        // terminality judgments about its children are final only when
        // neither holds.
        let child_revivable = self.meta.revivable
            || !matches!(
                self.children[key].runtime.definition.restart,
                crate::restart::RestartPolicy::Never
            );
        let nested_run = if matches!(&kind, ChildKind::Supervisor(_)) {
            let channels = nested_channels.ok_or_else(|| {
                SupervisorError::Internal(format!(
                    "missing stable channels for nested supervisor {child_id}"
                ))
            })?;
            let snapshot_state = snapshot_state.ok_or_else(|| {
                SupervisorError::Internal(format!(
                    "missing snapshot cell for nested supervisor {child_id}"
                ))
            })?;
            Some((
                ParentLink {
                    event_sink: EventSink::new(
                        self.nested_event_tx.clone(),
                        key,
                        instance,
                        self.meta.observability.clone(),
                    ),
                    snapshot_cell: SnapshotCell::new(
                        self.nested_snapshot_tx.clone(),
                        snapshot_state,
                        key,
                        instance,
                    ),
                    id: child_id.clone(),
                    generation,
                },
                channels,
                child_path_segments,
            ))
        } else {
            None
        };
        let child_path = self.meta.observability.child_path(&child_id);
        let supervisor_name = self.meta.observability.supervisor_name().to_owned();
        let supervisor_path = self.meta.observability.supervisor_path().to_owned();
        let child_span = info_span!(
            "child",
            supervisor_name = %supervisor_name,
            supervisor_path = %supervisor_path,
            child_id = %child_id,
            child_path = %child_path,
            generation,
        );

        let abort_handle = self.join_set.spawn(
            async move {
                let result = match kind {
                    ChildKind::Task(factory) => factory.make(ctx).await,
                    ChildKind::Supervisor(supervisor) => {
                        let (parent_link, channels, child_path_segments) =
                            nested_run.expect("nested run state validated before spawn");
                        supervisor
                            .run_as_child(
                                ctx,
                                parent_link,
                                channels,
                                child_path_segments,
                                child_revivable,
                            )
                            .await
                    }
                };
                if result.is_ok() {
                    let _ = completion_state.compare_exchange(
                        COMPLETION_PENDING,
                        COMPLETION_CLEAN,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                ChildEnvelope {
                    key,
                    instance,
                    generation,
                    result,
                }
            }
            .instrument(child_span),
        );
        let task_id = abort_handle.id();

        {
            let entry = self.children.get_mut(key).ok_or_else(|| {
                SupervisorError::Internal(format!("missing child slot for key {key}"))
            })?;
            let child = &mut entry.runtime;
            child.has_started = true;
            child.state = if child.definition.readiness == ChildReadiness::Immediate {
                child.has_reported_ready = true;
                RuntimeChildState::Running
            } else {
                RuntimeChildState::Starting
            };
            child.abort_handle = Some(abort_handle);
        }
        if !counted_before {
            self.running_children = self.running_children.saturating_add(1);
        }
        self.live_tasks = self.live_tasks.saturating_add(1);
        self.task_map.insert(
            task_id,
            TaskMeta {
                key,
                instance,
                generation,
            },
        );
        if self.children[key].runtime.state == RuntimeChildState::Running {
            self.send_event(SupervisorEvent::ChildStarted {
                id: child_id,
                generation,
            });
        } else {
            self.publish_snapshot();
        }

        Ok((old_generation, generation))
    }
}
