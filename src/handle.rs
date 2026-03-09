use std::sync::{Arc, Mutex};

use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};

use crate::{
    error::{SupervisorError, SupervisorExit},
    event::SupervisorEvent,
};

type SupervisorJoinHandle = JoinHandle<Result<SupervisorExit, SupervisorError>>;

#[derive(Clone)]
pub struct SupervisorHandle {
    pub(crate) shutdown_tx: watch::Sender<bool>,
    pub(crate) done_rx: watch::Receiver<Option<Result<SupervisorExit, SupervisorError>>>,
    pub(crate) done_tx: watch::Sender<Option<Result<SupervisorExit, SupervisorError>>>,
    pub(crate) events_tx: broadcast::Sender<SupervisorEvent>,
    pub(crate) join_handle: Arc<Mutex<Option<SupervisorJoinHandle>>>,
}

impl SupervisorHandle {
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn wait(&self) -> Result<SupervisorExit, SupervisorError> {
        if let Some(result) = self.done_rx.borrow().clone() {
            return result;
        }

        let join_handle = self
            .join_handle
            .lock()
            .expect("join_handle mutex poisoned")
            .take();

        if let Some(join_handle) = join_handle {
            let result = match join_handle.await {
                Ok(result) => result,
                Err(err) => Err(SupervisorError::Internal(format!(
                    "supervisor task failed to join: {err}"
                ))),
            };
            let _ = self.done_tx.send(Some(result.clone()));
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
}
