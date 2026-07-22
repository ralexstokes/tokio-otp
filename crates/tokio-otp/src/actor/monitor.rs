use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Notification that an actor incarnation has exited.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Down {
    /// Stable id of the actor that was watched.
    pub actor_id: String,
    /// Incarnation counter, starting at zero and increasing on every restart.
    pub generation: u64,
    /// How the watched incarnation exited.
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
}

/// Lifecycle transition of a watched logical actor.
///
/// Delivered by [`ActorContext::watch`](crate::ActorContext::watch). Events
/// for one watch arrive in lifecycle order: every [`Up`](Self::Up) for a
/// generation precedes its [`Down`](Self::Down), and
/// [`Terminated`](Self::Terminated) is final.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MonitorEvent {
    /// An incarnation of the watched actor is running.
    Up {
        /// Stable id of the watched actor.
        actor_id: String,
        /// Incarnation counter, starting at zero and increasing on every
        /// restart.
        generation: u64,
    },
    /// The current incarnation exited. If the supervisor restarts the actor,
    /// a matching [`Up`](Self::Up) follows.
    Down(Down),
    /// The actor is permanently gone. No further events will be delivered.
    Terminated {
        /// Stable id of the watched actor.
        actor_id: String,
        /// The last incarnation that ran, or `None` if the actor never
        /// started.
        generation: Option<u64>,
    },
}

/// A cancellable actor watch.
///
/// Clones refer to the same watch. Dropping the handle does not cancel the
/// watch; call [`cancel`](Self::cancel) explicitly. Watches are also
/// cancelled automatically when the observing actor stops or restarts.
#[derive(Clone, Debug)]
pub struct MonitorRef {
    pub(crate) cancellation: CancellationToken,
}

impl MonitorRef {
    /// Cancels the watch. Cancellation is idempotent.
    ///
    /// An event already accepted by the observer's mailbox cannot be
    /// retracted.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Returns whether this watch has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

struct Watcher {
    cancellation: CancellationToken,
    sender: mpsc::UnboundedSender<MonitorEvent>,
}

impl Watcher {
    fn is_live(&self) -> bool {
        !self.cancellation.is_cancelled() && !self.sender.is_closed()
    }

    fn notify(&self, event: &MonitorEvent) -> bool {
        !self.cancellation.is_cancelled() && self.sender.send(event.clone()).is_ok()
    }
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
    watchers: Vec<Watcher>,
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
                watchers: Vec::new(),
            }),
        }
    }

    /// Registers a persistent watch on this logical actor.
    ///
    /// A running target reports an immediate [`MonitorEvent::Up`] for the
    /// current incarnation. A target between incarnations (or before its
    /// first start) stays silent until the next start. A terminated target
    /// reports an immediate final [`MonitorEvent::Terminated`] and is not
    /// registered.
    ///
    /// Events are pushed while holding the hub lock, which totally orders
    /// them per watch; the sends are non-blocking unbounded-channel pushes,
    /// so no user code runs under the lock.
    pub(crate) fn register_watch(
        &self,
        cancellation: CancellationToken,
        sender: mpsc::UnboundedSender<MonitorEvent>,
    ) {
        let mut state = self.state();
        state.watchers.retain(Watcher::is_live);
        match state.lifecycle {
            Lifecycle::Terminated(generation) => {
                let _ = sender.send(self.terminated_event(generation));
                return;
            }
            Lifecycle::Running(generation) => {
                let _ = sender.send(self.up(generation));
            }
            Lifecycle::Pending | Lifecycle::Exited(_) => {}
        }
        state.watchers.push(Watcher {
            cancellation,
            sender,
        });
    }

    pub(crate) fn started(&self) {
        let mut state = self.state();
        let generation = state.next_generation;
        state.next_generation = state.next_generation.saturating_add(1);
        state.lifecycle = Lifecycle::Running(generation);
        let up = self.up(generation);
        state.watchers.retain(|watcher| watcher.notify(&up));
    }

    pub(crate) fn exited(&self, reason: DownReason) {
        let mut state = self.state();
        let Lifecycle::Running(generation) = state.lifecycle else {
            return;
        };
        state.lifecycle = Lifecycle::Exited(generation);
        let down = MonitorEvent::Down(self.down(generation, reason));
        state.watchers.retain(|watcher| watcher.notify(&down));
    }

    pub(crate) fn terminated(&self) {
        let mut state = self.state();
        let (down, generation) = match state.lifecycle {
            Lifecycle::Pending => (None, None),
            Lifecycle::Running(generation) => (
                Some(MonitorEvent::Down(
                    self.down(generation, DownReason::Failure),
                )),
                Some(generation),
            ),
            Lifecycle::Exited(generation) => (None, Some(generation)),
            Lifecycle::Terminated(_) => return,
        };
        state.lifecycle = Lifecycle::Terminated(generation);
        let terminated = self.terminated_event(generation);
        for watcher in state.watchers.drain(..) {
            if watcher.cancellation.is_cancelled() {
                continue;
            }
            if let Some(down) = &down {
                let _ = watcher.sender.send(down.clone());
            }
            let _ = watcher.sender.send(terminated.clone());
        }
    }

    fn up(&self, generation: u64) -> MonitorEvent {
        MonitorEvent::Up {
            actor_id: self.actor_id.clone(),
            generation,
        }
    }

    fn down(&self, generation: u64, reason: DownReason) -> Down {
        Down {
            actor_id: self.actor_id.clone(),
            generation,
            reason,
        }
    }

    fn terminated_event(&self, generation: Option<u64>) -> MonitorEvent {
        MonitorEvent::Terminated {
            actor_id: self.actor_id.clone(),
            generation,
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
