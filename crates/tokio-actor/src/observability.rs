use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use tracing::{Span, debug, info, info_span, trace, warn};

use crate::blocking::BlockingTaskId;

static NEXT_GRAPH_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn anonymous_graph_name() -> Arc<str> {
    format!("graph-{}", NEXT_GRAPH_ID.fetch_add(1, Ordering::Relaxed)).into()
}

#[derive(Clone, Debug)]
pub(crate) struct GraphObservability {
    graph_name: Arc<str>,
    running_actors: Arc<AtomicUsize>,
    running_blocking_tasks: Arc<AtomicUsize>,
}

impl GraphObservability {
    pub(crate) fn new(graph_name: Arc<str>) -> Self {
        Self {
            graph_name,
            running_actors: Arc::new(AtomicUsize::new(0)),
            running_blocking_tasks: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn graph_name(&self) -> &str {
        &self.graph_name
    }

    pub(crate) fn graph_span(
        &self,
        actor_count: usize,
        ingress_count: usize,
        mailbox_capacity: usize,
    ) -> Span {
        info_span!(
            "actor_graph",
            graph = %self.graph_name,
            actor_count,
            ingress_count,
            mailbox_capacity,
        )
    }

    pub(crate) fn actor_span(&self, actor_id: &str, peer_count: usize) -> Span {
        info_span!(
            "actor",
            graph = %self.graph_name,
            actor_id = %actor_id,
            peer_count,
        )
    }

    pub(crate) fn blocking_task_span(
        &self,
        actor_id: &str,
        task_id: BlockingTaskId,
        task_name: Option<&str>,
    ) -> Span {
        match task_name {
            Some(task_name) => info_span!(
                "blocking_task",
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                task_name = %task_name,
            ),
            None => info_span!(
                "blocking_task",
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
            ),
        }
    }

    pub(crate) fn emit_graph_started(
        &self,
        actor_count: usize,
        ingress_count: usize,
        mailbox_capacity: usize,
    ) {
        info!(
            graph = %self.graph_name,
            actor_count,
            ingress_count,
            mailbox_capacity,
            "graph started"
        );

        #[cfg(feature = "metrics")]
        counter!("actor_graph.runs.started", "graph" => self.graph_label()).increment(1);
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

        #[cfg(feature = "metrics")]
        {
            counter!(
                "actor_graph.runs.stopped",
                "graph" => self.graph_label(),
                "status" => status.as_str(),
            )
            .increment(1);
            histogram!(
                "actor_graph.run.duration",
                "graph" => self.graph_label(),
                "status" => status.as_str(),
            )
            .record(duration.as_secs_f64());
        }
    }

    pub(crate) fn emit_shutdown_requested(&self, cause: GraphShutdownCause) {
        debug!(
            graph = %self.graph_name,
            cause = cause.as_str(),
            "graph shutdown requested"
        );
    }

    pub(crate) fn emit_actor_started(&self, actor_id: &str) {
        let running = self.running_actors.fetch_add(1, Ordering::AcqRel) + 1;

        info!(
            graph = %self.graph_name,
            actor_id = %actor_id,
            running_actors = running,
            "actor started"
        );

        #[cfg(feature = "metrics")]
        {
            counter!(
                "actor_graph.actors.started",
                "graph" => self.graph_label(),
                "actor_id" => actor_id.to_owned(),
            )
            .increment(1);
            gauge!("actor_graph.actors.running", "graph" => self.graph_label()).set(running as f64);
        }
    }

    pub(crate) fn emit_actor_exited(
        &self,
        actor_id: &str,
        status: ActorExitStatus,
        error: Option<&str>,
    ) {
        let running = self.running_actors.fetch_sub(1, Ordering::AcqRel) - 1;

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

        #[cfg(feature = "metrics")]
        {
            counter!(
                "actor_graph.actors.exited",
                "graph" => self.graph_label(),
                "actor_id" => actor_id.to_owned(),
                "status" => status.as_str(),
            )
            .increment(1);
            gauge!("actor_graph.actors.running", "graph" => self.graph_label()).set(running as f64);
        }
    }

    pub(crate) fn emit_ingress_bound(&self, ingress: &str, actor_id: &str) {
        info!(
            graph = %self.graph_name,
            ingress = %ingress,
            actor_id = %actor_id,
            "ingress bound"
        );

        #[cfg(feature = "metrics")]
        gauge!(
            "actor_graph.ingress.bound",
            "graph" => self.graph_label(),
            "ingress" => ingress.to_owned(),
            "actor_id" => actor_id.to_owned(),
        )
        .set(1.0);
    }

    pub(crate) fn emit_ingress_cleared(&self, ingress: &str, actor_id: &str) {
        debug!(
            graph = %self.graph_name,
            ingress = %ingress,
            actor_id = %actor_id,
            "ingress cleared"
        );

        #[cfg(feature = "metrics")]
        gauge!(
            "actor_graph.ingress.bound",
            "graph" => self.graph_label(),
            "ingress" => ingress.to_owned(),
            "actor_id" => actor_id.to_owned(),
        )
        .set(0.0);
    }

    pub(crate) fn emit_actor_message(
        &self,
        source_actor_id: Option<&str>,
        target_actor_id: &str,
        operation: MessageOperation,
        envelope_len: usize,
        duration: Duration,
        rejection: Option<SendRejection>,
    ) {
        self.trace_actor_message(
            source_actor_id,
            target_actor_id,
            operation,
            envelope_len,
            duration,
            rejection,
        );

        #[cfg(feature = "metrics")]
        {
            match rejection {
                Some(rejection) => counter!(
                    "actor_graph.messages.rejected",
                    "graph" => self.graph_label(),
                    "actor_id" => target_actor_id.to_owned(),
                    "operation" => operation.as_str(),
                    "reason" => rejection.as_str(),
                )
                .increment(1),
                None => counter!(
                    "actor_graph.messages.sent",
                    "graph" => self.graph_label(),
                    "actor_id" => target_actor_id.to_owned(),
                    "operation" => operation.as_str(),
                )
                .increment(1),
            }
            histogram!(
                "actor_graph.send.duration",
                "graph" => self.graph_label(),
                "source" => "actor",
                "actor_id" => target_actor_id.to_owned(),
                "operation" => operation.as_str(),
                "status" => send_status_label(rejection),
            )
            .record(duration.as_secs_f64());
        }
    }

    pub(crate) fn emit_message_received(&self, actor_id: &str, envelope_len: usize) {
        trace!(
            graph = %self.graph_name,
            actor_id = %actor_id,
            envelope_len,
            "message received"
        );

        #[cfg(feature = "metrics")]
        counter!(
            "actor_graph.messages.received",
            "graph" => self.graph_label(),
            "actor_id" => actor_id.to_owned(),
        )
        .increment(1);
    }

    pub(crate) fn emit_ingress_message(
        &self,
        ingress: &str,
        actor_id: &str,
        operation: MessageOperation,
        envelope_len: usize,
        duration: Duration,
        rejection: Option<SendRejection>,
    ) {
        self.trace_ingress_message(
            ingress,
            actor_id,
            operation,
            envelope_len,
            duration,
            rejection,
        );

        #[cfg(feature = "metrics")]
        {
            match rejection {
                Some(rejection) => counter!(
                    "actor_graph.ingress.rejected",
                    "graph" => self.graph_label(),
                    "ingress" => ingress.to_owned(),
                    "actor_id" => actor_id.to_owned(),
                    "operation" => operation.as_str(),
                    "reason" => rejection.as_str(),
                )
                .increment(1),
                None => counter!(
                    "actor_graph.ingress.sent",
                    "graph" => self.graph_label(),
                    "ingress" => ingress.to_owned(),
                    "actor_id" => actor_id.to_owned(),
                    "operation" => operation.as_str(),
                )
                .increment(1),
            }
            histogram!(
                "actor_graph.send.duration",
                "graph" => self.graph_label(),
                "source" => "ingress",
                "ingress" => ingress.to_owned(),
                "actor_id" => actor_id.to_owned(),
                "operation" => operation.as_str(),
                "status" => send_status_label(rejection),
            )
            .record(duration.as_secs_f64());
        }
    }

    pub(crate) fn emit_blocking_task_started(
        &self,
        actor_id: &str,
        task_id: BlockingTaskId,
        task_name: Option<&str>,
    ) {
        let running = self.running_blocking_tasks.fetch_add(1, Ordering::AcqRel) + 1;

        match task_name {
            Some(task_name) => debug!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                task_name = %task_name,
                running_blocking_tasks = running,
                "blocking task started"
            ),
            None => debug!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                running_blocking_tasks = running,
                "blocking task started"
            ),
        }

        #[cfg(feature = "metrics")]
        {
            counter!(
                "actor_graph.blocking.started",
                "graph" => self.graph_label(),
                "actor_id" => actor_id.to_owned(),
            )
            .increment(1);
            gauge!("actor_graph.blocking.running", "graph" => self.graph_label())
                .set(running as f64);
        }
    }

    pub(crate) fn emit_blocking_task_finished(
        &self,
        actor_id: &str,
        task_id: BlockingTaskId,
        task_name: Option<&str>,
        status: BlockingTaskStatus,
        duration: Duration,
        error: Option<&str>,
    ) {
        let running = self.running_blocking_tasks.fetch_sub(1, Ordering::AcqRel) - 1;

        match (task_name, status, error) {
            (Some(task_name), BlockingTaskStatus::Completed | BlockingTaskStatus::Cancelled, _) => {
                debug!(
                    graph = %self.graph_name,
                    actor_id = %actor_id,
                    task_id = %task_id,
                    task_name = %task_name,
                    status = status.as_str(),
                    duration_secs = duration.as_secs_f64(),
                    running_blocking_tasks = running,
                    "blocking task finished"
                )
            }
            (Some(task_name), _, Some(error)) => warn!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                task_name = %task_name,
                status = status.as_str(),
                error = %error,
                duration_secs = duration.as_secs_f64(),
                running_blocking_tasks = running,
                "blocking task finished"
            ),
            (Some(task_name), _, None) => warn!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                task_name = %task_name,
                status = status.as_str(),
                duration_secs = duration.as_secs_f64(),
                running_blocking_tasks = running,
                "blocking task finished"
            ),
            (None, BlockingTaskStatus::Completed | BlockingTaskStatus::Cancelled, _) => debug!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                status = status.as_str(),
                duration_secs = duration.as_secs_f64(),
                running_blocking_tasks = running,
                "blocking task finished"
            ),
            (None, _, Some(error)) => warn!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                status = status.as_str(),
                error = %error,
                duration_secs = duration.as_secs_f64(),
                running_blocking_tasks = running,
                "blocking task finished"
            ),
            (None, _, None) => warn!(
                graph = %self.graph_name,
                actor_id = %actor_id,
                task_id = %task_id,
                status = status.as_str(),
                duration_secs = duration.as_secs_f64(),
                running_blocking_tasks = running,
                "blocking task finished"
            ),
        }

        #[cfg(feature = "metrics")]
        {
            counter!(
                "actor_graph.blocking.completed",
                "graph" => self.graph_label(),
                "actor_id" => actor_id.to_owned(),
                "status" => status.as_str(),
            )
            .increment(1);
            gauge!("actor_graph.blocking.running", "graph" => self.graph_label())
                .set(running as f64);
            histogram!(
                "actor_graph.blocking.duration",
                "graph" => self.graph_label(),
                "actor_id" => actor_id.to_owned(),
                "status" => status.as_str(),
            )
            .record(duration.as_secs_f64());
        }
    }

    fn trace_actor_message(
        &self,
        source_actor_id: Option<&str>,
        target_actor_id: &str,
        operation: MessageOperation,
        envelope_len: usize,
        duration: Duration,
        rejection: Option<SendRejection>,
    ) {
        match (source_actor_id, rejection) {
            (Some(source_actor_id), Some(rejection)) => trace!(
                graph = %self.graph_name,
                source_actor_id = %source_actor_id,
                actor_id = %target_actor_id,
                operation = operation.as_str(),
                reason = rejection.as_str(),
                envelope_len,
                duration_secs = duration.as_secs_f64(),
                "message rejected"
            ),
            (Some(source_actor_id), None) => trace!(
                graph = %self.graph_name,
                source_actor_id = %source_actor_id,
                actor_id = %target_actor_id,
                operation = operation.as_str(),
                envelope_len,
                duration_secs = duration.as_secs_f64(),
                "message sent"
            ),
            (None, Some(rejection)) => trace!(
                graph = %self.graph_name,
                actor_id = %target_actor_id,
                operation = operation.as_str(),
                reason = rejection.as_str(),
                envelope_len,
                duration_secs = duration.as_secs_f64(),
                "message rejected"
            ),
            (None, None) => trace!(
                graph = %self.graph_name,
                actor_id = %target_actor_id,
                operation = operation.as_str(),
                envelope_len,
                duration_secs = duration.as_secs_f64(),
                "message sent"
            ),
        }
    }

    fn trace_ingress_message(
        &self,
        ingress: &str,
        actor_id: &str,
        operation: MessageOperation,
        envelope_len: usize,
        duration: Duration,
        rejection: Option<SendRejection>,
    ) {
        match rejection {
            Some(rejection) => trace!(
                graph = %self.graph_name,
                ingress = %ingress,
                actor_id = %actor_id,
                operation = operation.as_str(),
                reason = rejection.as_str(),
                envelope_len,
                duration_secs = duration.as_secs_f64(),
                "ingress message rejected"
            ),
            None => trace!(
                graph = %self.graph_name,
                ingress = %ingress,
                actor_id = %actor_id,
                operation = operation.as_str(),
                envelope_len,
                duration_secs = duration.as_secs_f64(),
                "ingress message sent"
            ),
        }
    }

    #[cfg(feature = "metrics")]
    fn graph_label(&self) -> String {
        self.graph_name.to_string()
    }
}

fn send_status_label(rejection: Option<SendRejection>) -> &'static str {
    match rejection {
        Some(_) => "rejected",
        None => "ok",
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
    BlockingSend,
}

impl MessageOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::TrySend => "try_send",
            Self::BlockingSend => "blocking_send",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendRejection {
    UnknownPeer,
    MailboxFull,
    MailboxClosed,
    NotRunning,
}

impl SendRejection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::UnknownPeer => "unknown_peer",
            Self::MailboxFull => "mailbox_full",
            Self::MailboxClosed => "mailbox_closed",
            Self::NotRunning => "not_running",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockingTaskStatus {
    Completed,
    Cancelled,
    Failed,
    Panicked,
}

impl BlockingTaskStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Panicked => "panicked",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[cfg(feature = "metrics")]
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use tracing::{Level, info};
    use tracing_subscriber::{
        fmt::{self, MakeWriter, format::FmtSpan},
        prelude::*,
    };

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
    fn tracing_output_covers_graph_actor_ingress_message_and_blocking_events() {
        let observability = GraphObservability::new(Arc::from("orders"));

        assert_tracing_output(
            || observability.emit_graph_started(2, 1, 64),
            &[
                "graph started",
                r#""graph":"orders""#,
                r#""actor_count":2"#,
                r#""mailbox_capacity":64"#,
            ],
        );
        assert_tracing_output(
            || observability.emit_shutdown_requested(GraphShutdownCause::ActorFailed),
            &[
                "graph shutdown requested",
                r#""graph":"orders""#,
                r#""cause":"actor_failed""#,
            ],
        );
        assert_tracing_output(
            || observability.emit_actor_started("frontend"),
            &[
                "actor started",
                r#""graph":"orders""#,
                r#""actor_id":"frontend""#,
            ],
        );
        assert_tracing_output(
            || observability.emit_actor_exited("frontend", ActorExitStatus::Failed, Some("boom")),
            &[
                "actor exited",
                r#""actor_id":"frontend""#,
                r#""status":"failed""#,
                r#""error":"boom""#,
            ],
        );
        assert_tracing_output(
            || observability.emit_ingress_bound("requests", "frontend"),
            &[
                "ingress bound",
                r#""ingress":"requests""#,
                r#""actor_id":"frontend""#,
            ],
        );
        assert_tracing_output(
            || observability.emit_ingress_cleared("requests", "frontend"),
            &[
                "ingress cleared",
                r#""ingress":"requests""#,
                r#""actor_id":"frontend""#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_actor_message(
                    Some("frontend"),
                    "worker",
                    MessageOperation::Send,
                    5,
                    Duration::from_millis(2),
                    None,
                )
            },
            &[
                "message sent",
                r#""source_actor_id":"frontend""#,
                r#""actor_id":"worker""#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_actor_message(
                    Some("frontend"),
                    "worker",
                    MessageOperation::TrySend,
                    5,
                    Duration::from_millis(1),
                    Some(SendRejection::MailboxFull),
                )
            },
            &[
                "message rejected",
                r#""operation":"try_send""#,
                r#""reason":"mailbox_full""#,
            ],
        );
        assert_tracing_output(
            || observability.emit_message_received("worker", 5),
            &[
                "message received",
                r#""actor_id":"worker""#,
                r#""envelope_len":5"#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_ingress_message(
                    "requests",
                    "frontend",
                    MessageOperation::Send,
                    5,
                    Duration::from_millis(3),
                    None,
                )
            },
            &[
                "ingress message sent",
                r#""ingress":"requests""#,
                r#""actor_id":"frontend""#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_ingress_message(
                    "requests",
                    "frontend",
                    MessageOperation::Send,
                    5,
                    Duration::from_millis(1),
                    Some(SendRejection::NotRunning),
                )
            },
            &[
                "ingress message rejected",
                r#""ingress":"requests""#,
                r#""reason":"not_running""#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_blocking_task_started(
                    "worker",
                    BlockingTaskId::from_u64(7),
                    Some("io"),
                )
            },
            &[
                "blocking task started",
                r#""task_id":"7""#,
                r#""task_name":"io""#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_blocking_task_finished(
                    "worker",
                    BlockingTaskId::from_u64(7),
                    Some("io"),
                    BlockingTaskStatus::Failed,
                    Duration::from_millis(4),
                    Some("boom"),
                )
            },
            &[
                "blocking task finished",
                r#""status":"failed""#,
                r#""error":"boom""#,
            ],
        );
        assert_tracing_output(
            || {
                observability.emit_graph_stopped(
                    Duration::from_millis(10),
                    GraphRunStatus::Failed,
                    Some("actor `worker` returned an error"),
                )
            },
            &[
                "graph stopped",
                r#""status":"failed""#,
                r#""graph":"orders""#,
            ],
        );
    }

    #[test]
    fn tracing_output_covers_graph_actor_and_blocking_spans() {
        let observability = GraphObservability::new(Arc::from("orders"));

        let output = capture_tracing_output_with_spans(|| {
            let graph_span = observability.graph_span(2, 1, 64);
            let _graph_guard = graph_span.enter();

            let actor_span = observability.actor_span("frontend", 1);
            let _actor_guard = actor_span.enter();

            let blocking_span = observability.blocking_task_span(
                "frontend",
                BlockingTaskId::from_u64(3),
                Some("io"),
            );
            let _blocking_guard = blocking_span.enter();

            info!("inside spans");
        });

        for expected in [
            r#""name":"actor_graph""#,
            r#""graph":"orders""#,
            r#""actor_count":2"#,
            r#""name":"actor""#,
            r#""actor_id":"frontend""#,
            r#""name":"blocking_task""#,
            r#""task_id":"3""#,
        ] {
            assert!(
                output.contains(expected),
                "expected tracing output to contain `{expected}`, got: {output}"
            );
        }
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_cover_run_actor_message_ingress_and_blocking_series() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let observability = GraphObservability::new(Arc::from("orders"));

            observability.emit_graph_started(2, 1, 64);
            observability.emit_actor_started("frontend");
            observability.emit_actor_message(
                Some("frontend"),
                "worker",
                MessageOperation::Send,
                5,
                Duration::from_millis(2),
                None,
            );
            observability.emit_actor_message(
                Some("frontend"),
                "worker",
                MessageOperation::TrySend,
                5,
                Duration::from_millis(1),
                Some(SendRejection::MailboxFull),
            );
            observability.emit_message_received("worker", 5);
            observability.emit_ingress_bound("requests", "frontend");
            observability.emit_ingress_message(
                "requests",
                "frontend",
                MessageOperation::Send,
                5,
                Duration::from_millis(3),
                None,
            );
            observability.emit_ingress_message(
                "requests",
                "frontend",
                MessageOperation::Send,
                5,
                Duration::from_millis(1),
                Some(SendRejection::NotRunning),
            );
            observability.emit_blocking_task_started(
                "worker",
                BlockingTaskId::from_u64(1),
                Some("io"),
            );
            observability.emit_blocking_task_finished(
                "worker",
                BlockingTaskId::from_u64(1),
                Some("io"),
                BlockingTaskStatus::Failed,
                Duration::from_millis(4),
                Some("boom"),
            );
            observability.emit_actor_exited("frontend", ActorExitStatus::Shutdown, None);
            observability.emit_ingress_cleared("requests", "frontend");
            observability.emit_graph_stopped(Duration::from_millis(10), GraphRunStatus::Ok, None);
        });

        let metrics = snapshotter.snapshot().into_vec();

        assert_counter(
            &metrics,
            "actor_graph.runs.started",
            &[("graph", "orders")],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.runs.stopped",
            &[("graph", "orders"), ("status", "ok")],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.actors.started",
            &[("graph", "orders"), ("actor_id", "frontend")],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.actors.exited",
            &[
                ("graph", "orders"),
                ("actor_id", "frontend"),
                ("status", "shutdown"),
            ],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.messages.sent",
            &[
                ("graph", "orders"),
                ("actor_id", "worker"),
                ("operation", "send"),
            ],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.messages.rejected",
            &[
                ("graph", "orders"),
                ("actor_id", "worker"),
                ("operation", "try_send"),
                ("reason", "mailbox_full"),
            ],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.messages.received",
            &[("graph", "orders"), ("actor_id", "worker")],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.ingress.sent",
            &[
                ("graph", "orders"),
                ("ingress", "requests"),
                ("actor_id", "frontend"),
                ("operation", "send"),
            ],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.ingress.rejected",
            &[
                ("graph", "orders"),
                ("ingress", "requests"),
                ("actor_id", "frontend"),
                ("operation", "send"),
                ("reason", "not_running"),
            ],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.blocking.started",
            &[("graph", "orders"), ("actor_id", "worker")],
            1,
        );
        assert_counter(
            &metrics,
            "actor_graph.blocking.completed",
            &[
                ("graph", "orders"),
                ("actor_id", "worker"),
                ("status", "failed"),
            ],
            1,
        );
        assert_gauge(
            &metrics,
            "actor_graph.actors.running",
            &[("graph", "orders")],
            0.0,
        );
        assert_gauge(
            &metrics,
            "actor_graph.ingress.bound",
            &[
                ("graph", "orders"),
                ("ingress", "requests"),
                ("actor_id", "frontend"),
            ],
            0.0,
        );
        assert_gauge(
            &metrics,
            "actor_graph.blocking.running",
            &[("graph", "orders")],
            0.0,
        );
        assert_histogram_len(
            &metrics,
            "actor_graph.run.duration",
            &[("graph", "orders"), ("status", "ok")],
            1,
        );
        assert_histogram_len(
            &metrics,
            "actor_graph.send.duration",
            &[
                ("graph", "orders"),
                ("source", "actor"),
                ("actor_id", "worker"),
                ("operation", "send"),
                ("status", "ok"),
            ],
            1,
        );
        assert_histogram_len(
            &metrics,
            "actor_graph.send.duration",
            &[
                ("graph", "orders"),
                ("source", "ingress"),
                ("ingress", "requests"),
                ("actor_id", "frontend"),
                ("operation", "send"),
                ("status", "ok"),
            ],
            1,
        );
        assert_histogram_len(
            &metrics,
            "actor_graph.blocking.duration",
            &[
                ("graph", "orders"),
                ("actor_id", "worker"),
                ("status", "failed"),
            ],
            1,
        );
    }

    fn capture_tracing_output(f: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .json()
                .with_writer(buffer.clone())
                .with_current_span(false)
                .with_span_list(false)
                .without_time()
                .with_filter(tracing_subscriber::filter::LevelFilter::from_level(
                    Level::TRACE,
                )),
        );

        tracing::subscriber::with_default(subscriber, f);
        buffer.to_string_output()
    }

    fn capture_tracing_output_with_spans(f: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .json()
                .with_writer(buffer.clone())
                .with_current_span(true)
                .with_span_list(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .without_time()
                .with_filter(tracing_subscriber::filter::LevelFilter::from_level(
                    Level::TRACE,
                )),
        );

        tracing::subscriber::with_default(subscriber, f);
        buffer.to_string_output()
    }

    fn assert_tracing_output(f: impl FnOnce(), expected_fragments: &[&str]) {
        let output = capture_tracing_output(f);
        for expected in expected_fragments {
            assert!(
                output.contains(expected),
                "expected tracing output to contain `{expected}`, got: {output}"
            );
        }
    }

    #[derive(Clone, Default)]
    struct SharedBuffer {
        inner: Arc<Mutex<Vec<u8>>>,
    }

    impl SharedBuffer {
        fn to_string_output(&self) -> String {
            String::from_utf8(self.inner.lock().expect("buffer poisoned").clone())
                .expect("tracing output should be utf-8")
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriter {
                inner: Arc::clone(&self.inner),
            }
        }
    }

    struct SharedWriter {
        inner: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner
                .lock()
                .expect("buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(feature = "metrics")]
    fn assert_counter(
        metrics: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
        expected: u64,
    ) {
        let value = find_metric(metrics, name, labels);
        match value {
            DebugValue::Counter(actual) => assert_eq!(*actual, expected),
            other => panic!("expected counter for `{name}`, got {other:?}"),
        }
    }

    #[cfg(feature = "metrics")]
    fn assert_gauge(
        metrics: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
        expected: f64,
    ) {
        let value = find_metric(metrics, name, labels);
        match value {
            DebugValue::Gauge(actual) => assert_eq!(actual.into_inner(), expected),
            other => panic!("expected gauge for `{name}`, got {other:?}"),
        }
    }

    #[cfg(feature = "metrics")]
    fn assert_histogram_len(
        metrics: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
        expected: usize,
    ) {
        let value = find_metric(metrics, name, labels);
        match value {
            DebugValue::Histogram(values) => assert_eq!(values.len(), expected),
            other => panic!("expected histogram for `{name}`, got {other:?}"),
        }
    }

    #[cfg(feature = "metrics")]
    fn find_metric<'a>(
        metrics: &'a [(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
    ) -> &'a DebugValue {
        metrics
            .iter()
            .find_map(|(key, _, _, value)| {
                let metric_key = key.key();
                if metric_key.name() != name {
                    return None;
                }

                let actual_labels: Vec<(&str, &str)> = metric_key
                    .labels()
                    .map(|label| (label.key(), label.value()))
                    .collect();
                if labels
                    .iter()
                    .all(|expected| actual_labels.contains(expected))
                {
                    Some(value)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("missing metric `{name}` with labels {labels:?}"))
    }
}
