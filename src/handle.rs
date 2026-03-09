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
type DoneSender = watch::Sender<Option<Result<SupervisorExit, SupervisorError>>>;

#[derive(Clone)]
pub struct SupervisorHandle {
    shutdown_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<Option<Result<SupervisorExit, SupervisorError>>>,
    events_tx: broadcast::Sender<SupervisorEvent>,
    join_state: Arc<Mutex<Option<(SupervisorJoinHandle, DoneSender)>>>,
}

impl SupervisorHandle {
    pub(crate) fn new(
        shutdown_tx: watch::Sender<bool>,
        done_tx: DoneSender,
        done_rx: watch::Receiver<Option<Result<SupervisorExit, SupervisorError>>>,
        events_tx: broadcast::Sender<SupervisorEvent>,
        join_handle: SupervisorJoinHandle,
    ) -> Self {
        Self {
            shutdown_tx,
            done_rx,
            events_tx,
            join_state: Arc::new(Mutex::new(Some((join_handle, done_tx)))),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
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
}
