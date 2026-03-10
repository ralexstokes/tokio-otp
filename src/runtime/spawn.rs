use crate::{
    context::ChildContext,
    error::SupervisorError,
    event::{NestedEventForwarder, SupervisorEvent, with_nested_event_forwarder},
    runtime::{
        child_runtime::RuntimeChildState,
        supervision::{ChildEnvelope, SupervisorRuntime, TaskMeta},
    },
};

impl SupervisorRuntime {
    pub(crate) fn spawn_child(
        &mut self,
        idx: usize,
    ) -> Result<(Option<u64>, u64), SupervisorError> {
        self.clear_terminal_status(idx);
        let child = &mut self.children[idx];

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

        let ctx = ChildContext {
            id: self.child_names[idx].clone(),
            generation,
            token: child_token,
            supervisor_token: self.group_token.clone(),
        };
        let future = child.spec.factory.make(ctx);
        let forwarder = NestedEventForwarder::new(
            self.events.clone(),
            self.child_names[idx].clone(),
            generation,
        );

        let abort_handle = self.join_set.spawn(async move {
            let result = with_nested_event_forwarder(forwarder, future).await;
            ChildEnvelope {
                idx,
                generation,
                result,
            }
        });
        let task_id = abort_handle.id();

        child.has_started = true;
        child.state = RuntimeChildState::Running;
        child.abort_handle = Some(abort_handle);
        self.task_map.insert(task_id, TaskMeta { idx, generation });
        self.send_event(SupervisorEvent::ChildStarted {
            id: self.child_names[idx].clone(),
            generation,
        });

        Ok((old_generation, generation))
    }
}
