use crate::{
    context::{ChildContext, SupervisorToken},
    error::SupervisorError,
    event::{NestedEventForwarder, SupervisorEvent, with_nested_event_forwarder},
    handle::{NestedControlScope, with_nested_control_scope},
    runtime::{
        child_runtime::RuntimeChildState,
        supervision::{ChildEnvelope, SupervisorRuntime, TaskMeta},
    },
    snapshot::{NestedSnapshotForwarder, with_nested_snapshot_forwarder},
};
use tracing::{Instrument, info_span};

impl SupervisorRuntime {
    pub(crate) fn spawn_child(
        &mut self,
        key: usize,
    ) -> Result<(Option<u64>, u64), SupervisorError> {
        self.clear_terminal_status(key);
        let (
            child_id,
            generation,
            old_generation,
            future,
            counted_before,
            instance,
            snapshot_state,
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
            let child_token = self.group_token.child_token();
            child.active_token = Some(child_token.clone());
            child.state = RuntimeChildState::Starting;
            child.next_restart_deadline = None;
            entry.nested_snapshot = None;
            let snapshot_state = crate::snapshot::NestedSnapshotState::default();
            entry.nested_snapshot_state = Some(snapshot_state.clone());

            let child_id = entry.id.clone();
            let ctx = ChildContext {
                id: child_id.clone(),
                generation,
                token: child_token,
                supervisor_token: SupervisorToken::new(self.group_token.clone()),
            };
            let future = child.spec.factory.make(ctx);

            (
                child_id,
                generation,
                old_generation,
                future,
                counted_before,
                instance,
                snapshot_state,
            )
        };
        let forwarder =
            NestedEventForwarder::new(self.events.clone(), child_id.clone(), generation);
        let snapshot_forwarder = NestedSnapshotForwarder::new(
            self.nested_snapshot_tx.clone(),
            snapshot_state,
            key,
            instance,
            generation,
        );
        let control_scope =
            NestedControlScope::new(self.meta.registry.clone(), self.child_path(key));
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
                let result = with_nested_control_scope(control_scope, async move {
                    with_nested_snapshot_forwarder(snapshot_forwarder, async move {
                        with_nested_event_forwarder(forwarder, future).await
                    })
                    .await
                })
                .await;
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
            child.state = RuntimeChildState::Running;
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
        self.send_event(SupervisorEvent::ChildStarted {
            id: child_id,
            generation,
        });

        Ok((old_generation, generation))
    }
}
