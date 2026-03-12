use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{JoinHandle, spawn_blocking},
    time::{Instant as TokioInstant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    actor::BoxError,
    context::ActorRef,
    observability::{BlockingTaskStatus, GraphObservability},
};

/// Stable identifier for a blocking task owned by an actor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlockingTaskId(u64);

impl fmt::Display for BlockingTaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
impl BlockingTaskId {
    pub(crate) const fn from_u64(value: u64) -> Self {
        Self(value)
    }
}

/// Options used when spawning a blocking task from an actor.
#[derive(Clone, Debug, Default)]
pub struct BlockingOptions {
    name: Option<Cow<'static, str>>,
}

impl BlockingOptions {
    /// Creates a named blocking task option set.
    pub fn named(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: Some(name.into()),
        }
    }

    fn into_name(self) -> Option<Arc<str>> {
        self.name.map(|cow| Arc::from(cow.as_ref()))
    }
}

/// Errors returned by blocking task closures.
#[derive(Debug)]
pub enum BlockingOperationError {
    /// The blocking task observed cooperative cancellation.
    Cancelled,
    /// The blocking task returned an application error.
    Failed(BoxError),
}

impl fmt::Display for BlockingOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "blocking task was cancelled"),
            Self::Failed(_) => write!(f, "blocking task failed"),
        }
    }
}

impl<E> From<E> for BlockingOperationError
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(value: E) -> Self {
        Self::Failed(Box::new(value))
    }
}

/// Errors returned when spawning a blocking task from an actor.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpawnBlockingError {
    /// The actor is shutting down, so new blocking work is rejected.
    #[error("actor `{actor_id}` is shutting down")]
    ShuttingDown { actor_id: String },
    /// The actor has reached its active blocking task limit.
    #[error("actor `{actor_id}` has reached its blocking task limit of {max_blocking_tasks}")]
    AtCapacity {
        actor_id: String,
        max_blocking_tasks: usize,
    },
}

/// Shared, cloneable wrapper around an error returned by blocking work.
#[derive(Clone)]
pub struct SharedError(Arc<dyn std::error::Error + Send + Sync + 'static>);

impl From<BoxError> for SharedError {
    fn from(value: BoxError) -> Self {
        Self(Arc::from(value))
    }
}

impl fmt::Debug for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.0.as_ref(), f)
    }
}

impl fmt::Display for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.0.as_ref(), f)
    }
}

impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// Errors returned by blocking task handles.
#[derive(Clone, Debug, Error)]
pub enum BlockingTaskError {
    /// The blocking task could not be started.
    #[error(transparent)]
    Rejected(#[from] SpawnBlockingError),
    /// The blocking task observed cooperative cancellation.
    #[error("blocking task was cancelled")]
    Cancelled,
    /// The blocking task returned an application error.
    #[error("blocking task failed")]
    Failed(#[source] SharedError),
    /// The blocking task panicked.
    #[error("blocking task panicked")]
    Panicked,
    /// The task runtime lost the completion result unexpectedly.
    #[error("blocking task result became unavailable")]
    Unavailable,
}

/// A blocking task failure with task identity attached.
#[derive(Clone, Debug)]
pub struct BlockingTaskFailure {
    task_id: BlockingTaskId,
    task_name: Option<Arc<str>>,
    error: BlockingTaskError,
}

impl BlockingTaskFailure {
    pub(crate) fn new(
        task_id: BlockingTaskId,
        task_name: Option<Arc<str>>,
        error: BlockingTaskError,
    ) -> Self {
        Self {
            task_id,
            task_name,
            error,
        }
    }

    /// Returns the failed blocking task id.
    pub fn task_id(&self) -> BlockingTaskId {
        self.task_id
    }

    /// Returns the optional blocking task name.
    pub fn task_name(&self) -> Option<&str> {
        self.task_name.as_deref()
    }

    /// Returns the underlying blocking task error.
    pub fn error(&self) -> &BlockingTaskError {
        &self.error
    }
}

impl fmt::Display for BlockingTaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.task_name {
            write!(f, "blocking task `{}` (`{name}`) failed", self.task_id)
        } else {
            write!(f, "blocking task `{}` failed", self.task_id)
        }
    }
}

impl std::error::Error for BlockingTaskFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Synchronous context passed to blocking task closures.
pub struct BlockingContext {
    myself: ActorRef,
    actor_shutdown: CancellationToken,
    task_cancel: CancellationToken,
}

impl BlockingContext {
    /// Returns `true` if the task or its owning actor has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.task_cancel.is_cancelled() || self.actor_shutdown.is_cancelled()
    }

    /// Returns `Err(BlockingOperationError::Cancelled)` once the task should stop.
    pub fn checkpoint(&self) -> Result<(), BlockingOperationError> {
        if self.is_cancelled() {
            Err(BlockingOperationError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Returns a sender targeting the owning actor's mailbox.
    pub fn myself(&self) -> &ActorRef {
        &self.myself
    }
}

/// Handle to a blocking task spawned by an actor.
pub struct BlockingHandle {
    id: BlockingTaskId,
    name: Option<Arc<str>>,
    cancel: CancellationToken,
    state: Arc<BlockingTaskState>,
    completion_rx: oneshot::Receiver<Result<(), BlockingTaskError>>,
}

impl BlockingHandle {
    /// Returns the blocking task id.
    pub fn id(&self) -> BlockingTaskId {
        self.id
    }

    /// Returns the optional blocking task name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Requests cooperative cancellation of the blocking task.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Waits for the blocking task to complete.
    ///
    /// Awaiting the handle claims responsibility for the task result, so
    /// non-cancelled failures are returned here instead of also being
    /// propagated as an actor failure.
    pub async fn wait(self) -> Result<(), BlockingTaskError> {
        self.state.claim_for_wait();
        self.completion_rx
            .await
            .unwrap_or(Err(BlockingTaskError::Unavailable))
    }
}

impl fmt::Debug for BlockingHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockingHandle")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) struct BlockingSpawner {
    inner: Arc<BlockingRuntimeInner>,
}

impl BlockingSpawner {
    fn new(inner: Arc<BlockingRuntimeInner>) -> Self {
        Self { inner }
    }

    pub(crate) fn spawn_blocking<F>(
        &self,
        options: BlockingOptions,
        f: F,
    ) -> Result<BlockingHandle, SpawnBlockingError>
    where
        F: FnOnce(BlockingContext) -> Result<(), BlockingOperationError> + Send + 'static,
    {
        self.inner.spawn_blocking(options, f)
    }
}

pub(crate) struct BlockingRuntime {
    inner: Arc<BlockingRuntimeInner>,
    event_rx: mpsc::UnboundedReceiver<BlockingRuntimeEvent>,
    shutdown_timeout: std::time::Duration,
}

pub(crate) enum BlockingRuntimeEvent {
    Completed { task_id: BlockingTaskId },
}

impl BlockingRuntime {
    pub(crate) fn new(
        actor_id: Arc<str>,
        myself: ActorRef,
        actor_shutdown: CancellationToken,
        observability: GraphObservability,
        max_blocking_tasks: Option<usize>,
        shutdown_timeout: std::time::Duration,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            inner: Arc::new(BlockingRuntimeInner {
                actor_id,
                myself,
                actor_shutdown,
                state: Mutex::new(BlockingState::default()),
                event_tx,
                admission: max_blocking_tasks.map(BlockingAdmission::new),
                observability,
            }),
            event_rx,
            shutdown_timeout,
        }
    }

    pub(crate) fn spawner(&self) -> BlockingSpawner {
        BlockingSpawner::new(Arc::clone(&self.inner))
    }

    pub(crate) async fn next_event(&mut self) -> Option<BlockingRuntimeEvent> {
        self.event_rx.recv().await
    }

    pub(crate) async fn reap_task(&self, task_id: BlockingTaskId) -> Option<BlockingTaskFailure> {
        let entry = self.inner.take_task(task_id)?;
        self.await_entry(entry).await
    }

    pub(crate) async fn finish(
        &mut self,
        actor_result: Result<(), BoxError>,
    ) -> Result<(), BoxError> {
        let mut first_failure = None;
        let entries = self.inner.close_and_take_all();
        let deadline = TokioInstant::now() + self.shutdown_timeout;
        for entry in entries {
            Self::record_failure(
                &mut first_failure,
                self.await_entry_until(entry, deadline).await,
            );
        }

        match actor_result {
            Err(err) => Err(err),
            Ok(()) => match first_failure {
                Some(failure) => Err(Box::new(failure)),
                None => Ok(()),
            },
        }
    }

    fn record_failure(
        first_failure: &mut Option<BlockingTaskFailure>,
        failure: Option<BlockingTaskFailure>,
    ) {
        if first_failure.is_none() {
            *first_failure = failure;
        }
    }

    async fn await_entry(&self, entry: BlockingTaskEntry) -> Option<BlockingTaskFailure> {
        let BlockingTaskEntry {
            id,
            name,
            cancel: _cancel,
            state,
            join_handle,
        } = entry;
        Self::complete_entry(id, name, state, join_handle.await)
    }

    async fn await_entry_until(
        &self,
        entry: BlockingTaskEntry,
        deadline: TokioInstant,
    ) -> Option<BlockingTaskFailure> {
        let BlockingTaskEntry {
            id,
            name,
            cancel: _cancel,
            state,
            mut join_handle,
        } = entry;

        if TokioInstant::now() >= deadline {
            join_handle.abort();
            self.log_shutdown_timeout(id, name.as_deref());
            return None;
        }

        tokio::select! {
            joined = &mut join_handle => Self::complete_entry(id, name, state, joined),
            _ = sleep_until(deadline) => {
                join_handle.abort();
                self.log_shutdown_timeout(id, name.as_deref());
                None
            }
        }
    }

    fn complete_entry(
        id: BlockingTaskId,
        name: Option<Arc<str>>,
        state: Arc<BlockingTaskState>,
        join_result: Result<Option<BlockingTaskFailure>, tokio::task::JoinError>,
    ) -> Option<BlockingTaskFailure> {
        if state.was_claimed_for_wait() {
            return None;
        }

        match join_result {
            Ok(failure) => failure,
            Err(_) => Some(BlockingTaskFailure::new(
                id,
                name,
                BlockingTaskError::Unavailable,
            )),
        }
    }

    fn log_shutdown_timeout(&self, task_id: BlockingTaskId, task_name: Option<&str>) {
        match task_name {
            Some(task_name) => warn!(
                graph = %self.inner.observability.graph_name(),
                actor_id = %self.inner.actor_id,
                task_id = %task_id,
                task_name = %task_name,
                shutdown_timeout_secs = self.shutdown_timeout.as_secs_f64(),
                "blocking task did not stop before shutdown timeout; detaching task"
            ),
            None => warn!(
                graph = %self.inner.observability.graph_name(),
                actor_id = %self.inner.actor_id,
                task_id = %task_id,
                shutdown_timeout_secs = self.shutdown_timeout.as_secs_f64(),
                "blocking task did not stop before shutdown timeout; detaching task"
            ),
        }
    }
}

#[derive(Clone)]
struct BlockingAdmission {
    limit: usize,
    semaphore: Arc<Semaphore>,
}

impl BlockingAdmission {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            semaphore: Arc::new(Semaphore::new(limit)),
        }
    }

    fn try_acquire(&self, actor_id: &Arc<str>) -> Result<OwnedSemaphorePermit, SpawnBlockingError> {
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| SpawnBlockingError::AtCapacity {
                actor_id: actor_id.to_string(),
                max_blocking_tasks: self.limit,
            })
    }
}

struct BlockingRuntimeInner {
    actor_id: Arc<str>,
    myself: ActorRef,
    actor_shutdown: CancellationToken,
    state: Mutex<BlockingState>,
    event_tx: mpsc::UnboundedSender<BlockingRuntimeEvent>,
    admission: Option<BlockingAdmission>,
    observability: GraphObservability,
}

impl BlockingRuntimeInner {
    fn spawn_blocking<F>(
        &self,
        options: BlockingOptions,
        f: F,
    ) -> Result<BlockingHandle, SpawnBlockingError>
    where
        F: FnOnce(BlockingContext) -> Result<(), BlockingOperationError> + Send + 'static,
    {
        let task_name = options.into_name();
        let cancel = CancellationToken::new();
        let state = Arc::new(BlockingTaskState::default());
        let (completion_tx, completion_rx) = oneshot::channel();
        let started_at = Instant::now();

        let (id, permit) = {
            let mut state = self.lock_state();
            if state.closing || self.actor_shutdown.is_cancelled() {
                return Err(SpawnBlockingError::ShuttingDown {
                    actor_id: self.actor_id.to_string(),
                });
            }

            let permit = self
                .admission
                .as_ref()
                .map(|admission| admission.try_acquire(&self.actor_id))
                .transpose()?;
            let id = BlockingTaskId(state.next_id);
            state.next_id += 1;
            (id, permit)
        };

        let completion_name = task_name.clone();
        let completion_cancel = cancel.clone();
        let actor_shutdown = self.actor_shutdown.clone();
        let myself = self.myself.clone();
        let event_tx = self.event_tx.clone();
        let actor_id = Arc::clone(&self.actor_id);
        let observability = self.observability.clone();
        let span = observability.blocking_task_span(&actor_id, id, task_name.as_deref());

        let join_handle = spawn_blocking(move || {
            let _permit = permit;
            let _span_guard = span.enter();
            let ctx = BlockingContext {
                myself,
                actor_shutdown,
                task_cancel: completion_cancel,
            };
            let result = match catch_unwind(AssertUnwindSafe(|| f(ctx))) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(BlockingOperationError::Cancelled)) => Err(BlockingTaskError::Cancelled),
                Ok(Err(BlockingOperationError::Failed(err))) => {
                    Err(BlockingTaskError::Failed(SharedError::from(err)))
                }
                Err(_) => Err(BlockingTaskError::Panicked),
            };
            let status = blocking_status_for_result(&result);
            let error = result.as_ref().err().map(std::string::ToString::to_string);

            let failure = result
                .as_ref()
                .err()
                .filter(|error| !matches!(error, BlockingTaskError::Cancelled))
                .cloned()
                .map(|error| BlockingTaskFailure::new(id, completion_name.clone(), error));
            observability.emit_blocking_task_finished(
                &actor_id,
                id,
                completion_name.as_deref(),
                status,
                started_at.elapsed(),
                error.as_deref(),
            );
            let _ = completion_tx.send(result);
            let _ = event_tx.send(BlockingRuntimeEvent::Completed { task_id: id });
            failure
        });

        self.lock_state().tasks.insert(
            id,
            BlockingTaskEntry {
                id,
                name: task_name.clone(),
                cancel: cancel.clone(),
                state: Arc::clone(&state),
                join_handle,
            },
        );
        self.observability
            .emit_blocking_task_started(&self.actor_id, id, task_name.as_deref());

        Ok(BlockingHandle {
            id,
            name: task_name,
            cancel,
            state,
            completion_rx,
        })
    }

    fn close_and_take_all(&self) -> Vec<BlockingTaskEntry> {
        let mut state = self.lock_state();
        state.closing = true;
        for entry in state.tasks.values() {
            entry.cancel.cancel();
        }
        state.tasks.drain().map(|(_, entry)| entry).collect()
    }

    fn take_task(&self, task_id: BlockingTaskId) -> Option<BlockingTaskEntry> {
        self.lock_state().tasks.remove(&task_id)
    }

    fn lock_state(&self) -> MutexGuard<'_, BlockingState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[derive(Default)]
struct BlockingState {
    closing: bool,
    next_id: u64,
    tasks: HashMap<BlockingTaskId, BlockingTaskEntry>,
}

#[derive(Default)]
struct BlockingTaskState {
    claimed_for_wait: AtomicBool,
}

impl BlockingTaskState {
    fn claim_for_wait(&self) {
        self.claimed_for_wait.store(true, Ordering::Release);
    }

    fn was_claimed_for_wait(&self) -> bool {
        self.claimed_for_wait.load(Ordering::Acquire)
    }
}

struct BlockingTaskEntry {
    id: BlockingTaskId,
    name: Option<Arc<str>>,
    cancel: CancellationToken,
    state: Arc<BlockingTaskState>,
    join_handle: JoinHandle<Option<BlockingTaskFailure>>,
}

fn blocking_status_for_result(result: &Result<(), BlockingTaskError>) -> BlockingTaskStatus {
    match result {
        Ok(()) => BlockingTaskStatus::Completed,
        Err(BlockingTaskError::Cancelled) => BlockingTaskStatus::Cancelled,
        Err(BlockingTaskError::Failed(_)) => BlockingTaskStatus::Failed,
        Err(BlockingTaskError::Panicked | BlockingTaskError::Unavailable) => {
            BlockingTaskStatus::Panicked
        }
        Err(BlockingTaskError::Rejected(_)) => BlockingTaskStatus::Failed,
    }
}
