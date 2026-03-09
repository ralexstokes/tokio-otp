use tokio_util::sync::CancellationToken;

use crate::{
    context::ChildContext,
    error::SupervisorError,
    event::SupervisorEvent,
    runtime::{
        child_runtime::RuntimeChildState,
        supervision::{ChildEnvelope, SupervisorRuntime, TaskMeta},
    },
};

impl SupervisorRuntime {
    pub(crate) fn spawn_child(&mut self, id: &str) -> Result<(Option<u64>, u64), SupervisorError> {
        self.clear_terminal_status(id);
        let child = self
            .children
            .get_mut(id)
            .ok_or_else(|| SupervisorError::Internal(format!("unknown child: {id}")))?;

        let old_generation = if child.has_started {
            Some(child.generation)
        } else {
            None
        };
        if child.has_started {
            child.generation = child.generation.saturating_add(1);
        }

        let generation = child.generation;
        let child_token = CancellationToken::child_token(&self.group_token);
        child.active_token = Some(child_token.clone());
        child.state = RuntimeChildState::Starting;

        let ctx = ChildContext {
            id: id.to_owned(),
            generation,
            token: child_token,
            supervisor_token: self.group_token.clone(),
        };
        let future = child.spec.factory.make(ctx);
        let owned_id = id.to_owned();

        let abort_handle = self.join_set.spawn(async move {
            let result = future.await;
            ChildEnvelope {
                id: owned_id,
                generation,
                result,
            }
        });
        let task_id = abort_handle.id();

        child.has_started = true;
        child.state = RuntimeChildState::Running;
        child.abort_handle = Some(abort_handle);
        self.task_map.insert(
            task_id,
            TaskMeta {
                id: id.to_owned(),
                generation,
            },
        );
        self.send_event(SupervisorEvent::ChildStarted {
            id: id.to_owned(),
            generation,
        });

        Ok((old_generation, generation))
    }
}
