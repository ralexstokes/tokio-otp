use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tracing::{Span, debug, info, info_span, trace, warn};

static NEXT_GRAPH_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn anonymous_graph_name() -> Arc<str> {
    format!("graph-{}", NEXT_GRAPH_ID.fetch_add(1, Ordering::Relaxed)).into()
}

fn saturating_decrement(counter: &AtomicUsize) -> usize {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.saturating_sub(1);
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(actual) => current = actual,
        }
    }
}

/// Lifecycle tracing state retained by actor runners.
#[derive(Clone, Debug)]
pub(crate) struct GraphObservability {
    graph_name: Arc<str>,
    running_actors: Arc<AtomicUsize>,
}

impl GraphObservability {
    pub(crate) fn new(graph_name: Arc<str>) -> Self {
        Self {
            graph_name,
            running_actors: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn graph_span(&self, actor_count: usize, mailbox_capacity: usize) -> Span {
        info_span!(
            "actor_graph",
            graph = %self.graph_name,
            actor_count,
            mailbox_capacity,
        )
    }

    pub(crate) fn actor_span(&self, actor_id: &str) -> Span {
        info_span!("actor", graph = %self.graph_name, actor_id = %actor_id)
    }

    pub(crate) fn emit_graph_started(&self, actor_count: usize, mailbox_capacity: usize) {
        info!(
            graph = %self.graph_name,
            actor_count,
            mailbox_capacity,
            "graph started"
        );
    }

    pub(crate) fn emit_graph_stopped(
        &self,
        duration: Duration,
        status: GraphRunStatus,
        error: Option<&str>,
    ) {
        if status == GraphRunStatus::Ok {
            info!(
                graph = %self.graph_name,
                status = status.as_str(),
                duration_secs = duration.as_secs_f64(),
                "graph stopped"
            );
        } else if let Some(error) = error {
            warn!(
                graph = %self.graph_name,
                status = status.as_str(),
                error = %error,
                duration_secs = duration.as_secs_f64(),
                "graph stopped"
            );
        } else {
            warn!(
                graph = %self.graph_name,
                status = status.as_str(),
                duration_secs = duration.as_secs_f64(),
                "graph stopped"
            );
        }
    }

    pub(crate) fn emit_shutdown_requested(&self, cause: GraphShutdownCause) {
        debug!(
            graph = %self.graph_name,
            cause = cause.as_str(),
            "graph shutdown requested"
        );
    }

    pub(crate) fn emit_actor_started(&self, actor_id: &Arc<str>) {
        let running = self.running_actors.fetch_add(1, Ordering::AcqRel) + 1;
        info!(
            graph = %self.graph_name,
            actor_id = %actor_id,
            running_actors = running,
            "actor started"
        );
    }

    pub(crate) fn emit_actor_exited(
        &self,
        actor_id: &Arc<str>,
        status: ActorExitStatus,
        error: Option<&str>,
    ) {
        let running = saturating_decrement(&self.running_actors);
        if status == ActorExitStatus::Shutdown {
            debug!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                status = status.as_str(),
                running_actors = running,
                "actor exited"
            );
        } else if let Some(error) = error {
            warn!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                status = status.as_str(),
                error = %error,
                running_actors = running,
                "actor exited"
            );
        } else {
            warn!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                status = status.as_str(),
                running_actors = running,
                "actor exited"
            );
        }
    }

    pub(crate) fn emit_mailbox_bound(&self, actor_id: &Arc<str>) {
        info!(
            graph = %self.graph_name,
            actor_id = %actor_id,
            "actor mailbox bound"
        );
    }

    pub(crate) fn emit_mailbox_cleared(&self, actor_id: &Arc<str>) {
        debug!(
            graph = %self.graph_name,
            actor_id = %actor_id,
            "actor mailbox cleared"
        );
    }

    pub(crate) fn emit_message_received(&self, actor_id: &Arc<str>) {
        trace!(
            graph = %self.graph_name,
            actor_id = %actor_id,
            "message received"
        );
    }
}

/// Emits the per-message send trace without attaching graph-wide state to
/// restart-stable actor references.
pub(crate) fn trace_actor_message(
    source_actor_id: Option<&str>,
    target_actor_id: &Arc<str>,
    operation: MessageOperation,
    rejection: Option<SendRejection>,
) {
    match (source_actor_id, rejection) {
        (Some(source_actor_id), Some(rejection)) => trace!(
            source_actor_id = %source_actor_id,
            actor_id = %target_actor_id,
            operation = operation.as_str(),
            reason = rejection.as_str(),
            "message rejected"
        ),
        (Some(source_actor_id), None) => trace!(
            source_actor_id = %source_actor_id,
            actor_id = %target_actor_id,
            operation = operation.as_str(),
            "message sent"
        ),
        (None, Some(rejection)) => trace!(
            actor_id = %target_actor_id,
            operation = operation.as_str(),
            reason = rejection.as_str(),
            "message rejected"
        ),
        (None, None) => trace!(
            actor_id = %target_actor_id,
            operation = operation.as_str(),
            "message sent"
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphRunStatus {
    Ok,
    Failed,
}

impl GraphRunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphShutdownCause {
    External,
    ActorStopped,
    ActorFailed,
    ActorPanicked,
    ActorCancelled,
}

impl GraphShutdownCause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::ActorStopped => "actor_stopped",
            Self::ActorFailed => "actor_failed",
            Self::ActorPanicked => "actor_panicked",
            Self::ActorCancelled => "actor_cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActorExitStatus {
    Shutdown,
    Stopped,
    Failed,
    Panicked,
    Cancelled,
}

impl ActorExitStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Panicked => "panicked",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageOperation {
    Send,
    TrySend,
}

impl MessageOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::TrySend => "try_send",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendRejection {
    MailboxFull,
    MailboxClosed,
    NotRunning,
    ActorTerminated,
}

impl SendRejection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MailboxFull => "mailbox_full",
            Self::MailboxClosed => "mailbox_closed",
            Self::NotRunning => "not_running",
            Self::ActorTerminated => "actor_terminated",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_graph_names_are_unique() {
        let first = anonymous_graph_name();
        let second = anonymous_graph_name();
        assert_ne!(first, second);
        assert!(first.starts_with("graph-"));
        assert!(second.starts_with("graph-"));
    }

    #[test]
    fn actor_exit_running_count_saturates_at_zero() {
        let observability = GraphObservability::new(Arc::from("orders"));
        observability.emit_actor_exited(&Arc::from("worker"), ActorExitStatus::Cancelled, None);
        assert_eq!(observability.running_actors.load(Ordering::Acquire), 0);
    }
}
