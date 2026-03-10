use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};

use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{
    child::ChildSpec,
    error::{ControlError, SupervisorError, SupervisorExit},
    event::SupervisorEvent,
    snapshot::SupervisorSnapshot,
};

type SupervisorJoinHandle = JoinHandle<Result<SupervisorExit, SupervisorError>>;
type DoneSender = watch::Sender<Option<Result<SupervisorExit, SupervisorError>>>;
type DoneReceiver = watch::Receiver<Option<Result<SupervisorExit, SupervisorError>>>;

#[derive(Clone)]
pub(crate) struct ControlEndpoint {
    command_tx: mpsc::Sender<SupervisorCommand>,
}

impl ControlEndpoint {
    async fn add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.send(|reply| SupervisorCommand::AddChild { child, reply })
            .await
    }

    async fn try_add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.try_send(|reply| SupervisorCommand::AddChild { child, reply })
            .await
    }

    async fn remove_child(&self, id: String) -> Result<(), ControlError> {
        self.send(|reply| SupervisorCommand::RemoveChild { id, reply })
            .await
    }

    async fn try_remove_child(&self, id: String) -> Result<(), ControlError> {
        self.try_send(|reply| SupervisorCommand::RemoveChild { id, reply })
            .await
    }

    async fn send(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<(), ControlError>>) -> SupervisorCommand,
    ) -> Result<(), ControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(command(reply_tx))
            .await
            .map_err(|_| ControlError::Unavailable)?;
        reply_rx.await.map_err(|_| ControlError::Unavailable)?
    }

    async fn try_send(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<(), ControlError>>) -> SupervisorCommand,
    ) -> Result<(), ControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .try_send(command(reply_tx))
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => ControlError::Busy,
                mpsc::error::TrySendError::Closed(_) => ControlError::Unavailable,
            })?;
        reply_rx.await.map_err(|_| ControlError::Unavailable)?
    }
}

#[derive(Default)]
pub(crate) struct NestedControlRegistry {
    endpoints: Mutex<HashMap<Vec<String>, ControlEndpoint>>,
}

impl NestedControlRegistry {
    fn insert(&self, path: Vec<String>, endpoint: ControlEndpoint) {
        self.endpoints
            .lock()
            .expect("nested control registry mutex poisoned")
            .insert(path, endpoint);
    }

    fn remove(&self, path: &[String]) {
        self.endpoints
            .lock()
            .expect("nested control registry mutex poisoned")
            .remove(path);
    }

    fn get(&self, path: &[String]) -> Option<ControlEndpoint> {
        self.endpoints
            .lock()
            .expect("nested control registry mutex poisoned")
            .get(path)
            .cloned()
    }
}

#[derive(Clone)]
pub(crate) struct NestedControlScope {
    registry: Arc<NestedControlRegistry>,
    child_path: Vec<String>,
}

impl NestedControlScope {
    pub(crate) fn new(registry: Arc<NestedControlRegistry>, child_path: Vec<String>) -> Self {
        Self {
            registry,
            child_path,
        }
    }

    pub(crate) fn registry(&self) -> Arc<NestedControlRegistry> {
        Arc::clone(&self.registry)
    }

    pub(crate) fn child_path(&self) -> Vec<String> {
        self.child_path.clone()
    }

    pub(crate) fn register(&self, endpoint: ControlEndpoint) -> NestedControlRegistration {
        self.registry.insert(self.child_path.clone(), endpoint);
        NestedControlRegistration {
            registry: Arc::clone(&self.registry),
            child_path: self.child_path.clone(),
        }
    }
}

pub(crate) struct NestedControlRegistration {
    registry: Arc<NestedControlRegistry>,
    child_path: Vec<String>,
}

impl Drop for NestedControlRegistration {
    fn drop(&mut self) {
        self.registry.remove(&self.child_path);
    }
}

tokio::task_local! {
    static NESTED_CONTROL_SCOPE: NestedControlScope;
}

pub(crate) async fn with_nested_control_scope<Fut>(
    scope: NestedControlScope,
    future: Fut,
) -> Fut::Output
where
    Fut: Future,
{
    NESTED_CONTROL_SCOPE.scope(scope, future).await
}

pub(crate) fn current_nested_control_scope() -> Option<NestedControlScope> {
    NESTED_CONTROL_SCOPE.try_with(Clone::clone).ok()
}

pub(crate) enum SupervisorCommand {
    AddChild {
        child: ChildSpec,
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    RemoveChild {
        id: String,
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
}

pub(crate) struct SupervisorHandleInit {
    pub(crate) shutdown_tx: watch::Sender<bool>,
    pub(crate) command_tx: mpsc::Sender<SupervisorCommand>,
    pub(crate) registry: Arc<NestedControlRegistry>,
    pub(crate) path_prefix: Vec<String>,
    pub(crate) done_tx: DoneSender,
    pub(crate) done_rx: DoneReceiver,
    pub(crate) events_tx: broadcast::Sender<SupervisorEvent>,
    pub(crate) snapshots_rx: watch::Receiver<SupervisorSnapshot>,
    pub(crate) join_handle: SupervisorJoinHandle,
}

#[derive(Clone)]
pub struct SupervisorHandle {
    shutdown_tx: watch::Sender<bool>,
    command_tx: mpsc::Sender<SupervisorCommand>,
    registry: Arc<NestedControlRegistry>,
    path_prefix: Vec<String>,
    done_rx: DoneReceiver,
    events_tx: broadcast::Sender<SupervisorEvent>,
    snapshots_rx: watch::Receiver<SupervisorSnapshot>,
    join_state: Arc<Mutex<Option<(SupervisorJoinHandle, DoneSender)>>>,
}

impl SupervisorHandle {
    pub(crate) fn new(init: SupervisorHandleInit) -> Self {
        Self {
            shutdown_tx: init.shutdown_tx,
            command_tx: init.command_tx,
            registry: init.registry,
            path_prefix: init.path_prefix,
            done_rx: init.done_rx,
            events_tx: init.events_tx,
            snapshots_rx: init.snapshots_rx,
            join_state: Arc::new(Mutex::new(Some((init.join_handle, init.done_tx)))),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn shutdown_and_wait(&self) -> Result<SupervisorExit, SupervisorError> {
        self.shutdown();
        self.wait().await
    }

    pub async fn add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.control_endpoint().add_child(child).await
    }

    pub async fn try_add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.control_endpoint().try_add_child(child).await
    }

    pub async fn add_child_at<I, S>(&self, path: I, child: ChildSpec) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if let Some(endpoint) = self.endpoint_for_relative_path(path)? {
            endpoint.add_child(child).await
        } else {
            self.add_child(child).await
        }
    }

    pub async fn try_add_child_at<I, S>(
        &self,
        path: I,
        child: ChildSpec,
    ) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if let Some(endpoint) = self.endpoint_for_relative_path(path)? {
            endpoint.try_add_child(child).await
        } else {
            self.try_add_child(child).await
        }
    }

    pub async fn remove_child(&self, id: impl Into<String>) -> Result<(), ControlError> {
        self.control_endpoint().remove_child(id.into()).await
    }

    pub async fn try_remove_child(&self, id: impl Into<String>) -> Result<(), ControlError> {
        self.control_endpoint().try_remove_child(id.into()).await
    }

    pub async fn remove_child_at<I, S>(
        &self,
        path: I,
        id: impl Into<String>,
    ) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if let Some(endpoint) = self.endpoint_for_relative_path(path)? {
            endpoint.remove_child(id.into()).await
        } else {
            self.remove_child(id).await
        }
    }

    pub async fn try_remove_child_at<I, S>(
        &self,
        path: I,
        id: impl Into<String>,
    ) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if let Some(endpoint) = self.endpoint_for_relative_path(path)? {
            endpoint.try_remove_child(id.into()).await
        } else {
            self.try_remove_child(id).await
        }
    }

    pub async fn wait(&self) -> Result<SupervisorExit, SupervisorError> {
        if let Some(result) = self.done_rx.borrow().clone() {
            return result;
        }

        let join_state = self
            .join_state
            .lock()
            .expect("join_state mutex poisoned")
            .take();

        if let Some((join_handle, done_tx)) = join_state {
            let result = match join_handle.await {
                Ok(result) => result,
                Err(err) => Err(SupervisorError::Internal(format!(
                    "supervisor task failed to join: {err}"
                ))),
            };
            let _ = done_tx.send(Some(result.clone()));
            return result;
        }

        let mut done_rx = self.done_rx.clone();
        done_rx
            .wait_for(|value| value.is_some())
            .await
            .map_err(|_| {
                SupervisorError::Internal("supervisor completion channel closed".to_owned())
            })?;

        done_rx.borrow().clone().unwrap_or_else(|| {
            Err(SupervisorError::Internal(
                "missing supervisor completion result".to_owned(),
            ))
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.events_tx.subscribe()
    }

    pub fn snapshot(&self) -> SupervisorSnapshot {
        self.snapshots_rx.borrow().clone()
    }

    pub fn subscribe_snapshots(&self) -> watch::Receiver<SupervisorSnapshot> {
        self.snapshots_rx.clone()
    }

    pub(crate) fn control_endpoint(&self) -> ControlEndpoint {
        ControlEndpoint {
            command_tx: self.command_tx.clone(),
        }
    }

    fn endpoint_for_relative_path<I, S>(
        &self,
        path: I,
    ) -> Result<Option<ControlEndpoint>, ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let path = collect_path(path);
        if path.is_empty() {
            Ok(None)
        } else {
            self.endpoint_for_path(&path).map(Some)
        }
    }

    fn endpoint_for_path(&self, relative_path: &[String]) -> Result<ControlEndpoint, ControlError> {
        let mut absolute_path = self.path_prefix.clone();
        absolute_path.extend(relative_path.iter().cloned());

        self.registry
            .get(&absolute_path)
            .ok_or_else(|| ControlError::UnknownChildId(absolute_path.join(".")))
    }
}

fn collect_path<I, S>(path: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    path.into_iter()
        .map(|segment| segment.as_ref().to_owned())
        .collect()
}
