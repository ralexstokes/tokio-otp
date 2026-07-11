use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio_util::sync::CancellationToken;

/// Notification that an actor incarnation has exited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Down {
    /// Stable id of the actor that was monitored.
    pub actor_id: String,
    /// Incarnation counter, starting at zero and increasing on every restart.
    /// This value is a placeholder when [`reason`](Self::reason) is
    /// [`NoProcess`](DownReason::NoProcess) for an actor that never started.
    pub generation: u64,
    /// How the monitored incarnation exited.
    pub reason: DownReason,
}

/// The reason carried by a [`Down`] notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DownReason {
    /// The actor stopped cleanly or as part of an orderly shutdown.
    Normal,
    /// The actor failed, panicked, or was aborted.
    Failure,
    /// No live incarnation existed when the monitor was registered.
    NoProcess,
}

/// A cancellable actor monitor.
///
/// Clones refer to the same monitor. Dropping the handle does not cancel the
/// monitor; call [`cancel`](Self::cancel) explicitly. Monitors are also
/// cancelled automatically when the observing actor stops or restarts.
#[derive(Clone, Debug)]
pub struct MonitorRef {
    pub(crate) cancellation: CancellationToken,
}

impl MonitorRef {
    /// Cancels the monitor. Cancellation is idempotent.
    ///
    /// A notification already accepted by the observer's mailbox cannot be
    /// retracted.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Returns whether this monitor has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

type MonitorCallback = Box<dyn FnOnce(Down) + Send + 'static>;

struct Registration {
    generation: u64,
    cancellation: CancellationToken,
    callback: MonitorCallback,
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Pending,
    Running(u64),
    Exited(u64),
    Terminated(Option<u64>),
}

struct MonitorState {
    next_generation: u64,
    lifecycle: Lifecycle,
    next_registration: u64,
    registrations: HashMap<u64, Registration>,
}

pub(crate) struct MonitorHub {
    actor_id: String,
    state: Mutex<MonitorState>,
}

impl MonitorHub {
    pub(crate) fn new(actor_id: &str) -> Self {
        Self {
            actor_id: actor_id.to_owned(),
            state: Mutex::new(MonitorState {
                next_generation: 0,
                lifecycle: Lifecycle::Pending,
                next_registration: 0,
                registrations: HashMap::new(),
            }),
        }
    }

    pub(crate) fn register(&self, cancellation: CancellationToken, callback: MonitorCallback) {
        let mut state = self.state();
        state
            .registrations
            .retain(|_, registration| !registration.cancellation.is_cancelled());
        let generation = match state.lifecycle {
            Lifecycle::Pending => state.next_generation,
            Lifecycle::Running(generation) => generation,
            Lifecycle::Exited(generation) | Lifecycle::Terminated(Some(generation)) => {
                drop(state);
                callback(self.down(generation, DownReason::NoProcess));
                return;
            }
            Lifecycle::Terminated(None) => {
                drop(state);
                callback(self.down(0, DownReason::NoProcess));
                return;
            }
        };
        let id = state.next_registration;
        state.next_registration = state.next_registration.wrapping_add(1);
        state.registrations.insert(
            id,
            Registration {
                generation,
                cancellation,
                callback,
            },
        );
    }

    pub(crate) fn started(&self) {
        let mut state = self.state();
        state
            .registrations
            .retain(|_, registration| !registration.cancellation.is_cancelled());
        let generation = state.next_generation;
        state.next_generation = state.next_generation.saturating_add(1);
        state.lifecycle = Lifecycle::Running(generation);
    }

    pub(crate) fn exited(&self, reason: DownReason) {
        let callbacks = {
            let mut state = self.state();
            let Lifecycle::Running(generation) = state.lifecycle else {
                return;
            };
            state.lifecycle = Lifecycle::Exited(generation);
            let down = self.down(generation, reason);
            state
                .registrations
                .extract_if(|_, registration| registration.generation == generation)
                .map(|(_, registration)| (registration, down.clone()))
                .collect::<Vec<_>>()
        };

        for (registration, down) in callbacks {
            if !registration.cancellation.is_cancelled() {
                (registration.callback)(down);
            }
        }
    }

    pub(crate) fn terminated(&self) {
        let callbacks = {
            let mut state = self.state();
            match state.lifecycle {
                Lifecycle::Pending => {
                    state.lifecycle = Lifecycle::Terminated(None);
                    let down = self.down(0, DownReason::NoProcess);
                    state
                        .registrations
                        .drain()
                        .map(|(_, registration)| (registration, down.clone()))
                        .collect::<Vec<_>>()
                }
                Lifecycle::Running(generation) => {
                    state.lifecycle = Lifecycle::Terminated(Some(generation));
                    let down = self.down(generation, DownReason::Failure);
                    state
                        .registrations
                        .extract_if(|_, registration| registration.generation == generation)
                        .map(|(_, registration)| (registration, down.clone()))
                        .collect::<Vec<_>>()
                }
                Lifecycle::Exited(generation) => {
                    state.lifecycle = Lifecycle::Terminated(Some(generation));
                    Vec::new()
                }
                Lifecycle::Terminated(_) => Vec::new(),
            }
        };

        for (registration, down) in callbacks {
            if !registration.cancellation.is_cancelled() {
                (registration.callback)(down);
            }
        }
    }

    fn down(&self, generation: u64, reason: DownReason) -> Down {
        Down {
            actor_id: self.actor_id.clone(),
            generation,
            reason,
        }
    }

    fn state(&self) -> MutexGuard<'_, MonitorState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct MonitorExitGuard {
    hub: Arc<MonitorHub>,
    reported: bool,
}

impl MonitorExitGuard {
    pub(crate) fn new(hub: Arc<MonitorHub>) -> Self {
        Self {
            hub,
            reported: false,
        }
    }

    pub(crate) fn report(&mut self, reason: DownReason) {
        self.hub.exited(reason);
        self.reported = true;
    }
}

impl Drop for MonitorExitGuard {
    fn drop(&mut self) {
        if !self.reported {
            self.hub.exited(DownReason::Failure);
        }
    }
}

pub(crate) struct ActorMonitors(CancellationToken);

impl ActorMonitors {
    pub(crate) fn new(shutdown: &CancellationToken) -> Self {
        Self(shutdown.child_token())
    }

    pub(crate) fn child_token(&self) -> CancellationToken {
        self.0.child_token()
    }
}

impl Drop for ActorMonitors {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
