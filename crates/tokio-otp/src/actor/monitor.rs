use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{Notify, futures::Notified};
use tokio_util::sync::CancellationToken;

/// Maximum number of undelivered events staged for a single watch.
///
/// Lifecycle events are rare in normal operation (one per restart cycle), so
/// this bound is only reached when an observer's mailbox stays full while its
/// target restarts in a tight loop. Beyond the bound the oldest staged event
/// is dropped, which coalesces a restart storm into recent history plus the
/// current state; the terminal [`MonitorEvent::Terminated`] is always the
/// newest event, so it is never dropped. This caps the memory a stalled
/// observer can pin regardless of how fast its target churns.
const WATCH_BUFFER_CAP: usize = 128;

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

/// Bounded, drop-oldest staging buffer between a target's [`MonitorHub`] and
/// one observer's forwarder task.
///
/// The hub pushes events without awaiting (it holds its own lock); the
/// forwarder drains them and applies the observer's mailbox backpressure. The
/// bound lives here rather than in the mailbox because the hub cannot block on
/// a full mailbox, so an unbounded hand-off would let a churning target pin
/// arbitrary memory behind a stalled observer.
pub(crate) struct WatchQueue {
    events: Mutex<VecDeque<MonitorEvent>>,
    notify: Notify,
    closed: AtomicBool,
}

impl WatchQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        })
    }

    fn events(&self) -> MutexGuard<'_, VecDeque<MonitorEvent>> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stages one event, dropping the oldest if the buffer is full. The
    /// terminal event is always the newest, so overflow never drops it.
    fn push(&self, event: MonitorEvent) {
        {
            let mut events = self.events();
            if events.len() >= WATCH_BUFFER_CAP {
                events.pop_front();
            }
            events.push_back(event);
        }
        self.notify.notify_one();
    }

    /// Removes the next staged event, if any. Called only by the forwarder.
    pub(crate) fn pop(&self) -> Option<MonitorEvent> {
        self.events().pop_front()
    }

    /// A future that resolves when an event may be waiting. Arm it before
    /// observing an empty queue to avoid a lost wake-up.
    pub(crate) fn waiter(&self) -> Notified<'_> {
        self.notify.notified()
    }

    /// Marks the forwarder gone so the hub stops staging into a dead queue.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

struct Watcher {
    cancellation: CancellationToken,
    queue: Arc<WatchQueue>,
}

impl Watcher {
    fn is_live(&self) -> bool {
        !self.cancellation.is_cancelled() && !self.queue.is_closed()
    }

    fn notify(&self, event: &MonitorEvent) -> bool {
        if !self.is_live() {
            return false;
        }
        self.queue.push(event.clone());
        true
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

    /// Registers a persistent watch on this logical actor and returns its
    /// staging queue for the caller's forwarder to drain.
    ///
    /// A running target stages an immediate [`MonitorEvent::Up`] for the
    /// current incarnation. A target between incarnations (or before its
    /// first start) stays silent until the next start. A terminated target
    /// stages an immediate final [`MonitorEvent::Terminated`] and is not
    /// registered.
    ///
    /// Events are staged while holding the hub lock, which totally orders
    /// them per watch; staging is a non-blocking buffer push, so no user code
    /// runs under the lock.
    pub(crate) fn register_watch(&self, cancellation: CancellationToken) -> Arc<WatchQueue> {
        let queue = WatchQueue::new();
        let mut state = self.state();
        state.watchers.retain(Watcher::is_live);
        match state.lifecycle {
            Lifecycle::Terminated(generation) => {
                queue.push(self.terminated_event(generation));
                return queue;
            }
            Lifecycle::Running(generation) => {
                queue.push(self.up(generation));
            }
            Lifecycle::Pending | Lifecycle::Exited(_) => {}
        }
        state.watchers.push(Watcher {
            cancellation,
            queue: Arc::clone(&queue),
        });
        queue
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
                watcher.queue.push(down.clone());
            }
            watcher.queue.push(terminated.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn up_event(generation: u64) -> MonitorEvent {
        MonitorEvent::Up {
            actor_id: "peer".to_owned(),
            generation,
        }
    }

    #[test]
    fn queue_drops_oldest_beyond_cap() {
        let queue = WatchQueue::new();
        let overflow = 5;
        for generation in 0..(WATCH_BUFFER_CAP as u64 + overflow) {
            queue.push(up_event(generation));
        }

        assert_eq!(queue.events().len(), WATCH_BUFFER_CAP);
        // The oldest `overflow` events were dropped; the surviving front is
        // the first event that still fits the bound.
        assert_eq!(queue.pop(), Some(up_event(overflow)));
    }

    #[test]
    fn terminal_event_survives_overflow() {
        let queue = WatchQueue::new();
        for generation in 0..(WATCH_BUFFER_CAP as u64 * 2) {
            queue.push(up_event(generation));
        }
        let terminated = MonitorEvent::Terminated {
            actor_id: "peer".to_owned(),
            generation: Some(7),
        };
        queue.push(terminated.clone());

        let mut last = None;
        while let Some(event) = queue.pop() {
            last = Some(event);
        }
        assert_eq!(last, Some(terminated));
    }
}
