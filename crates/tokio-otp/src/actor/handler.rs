use std::future::Future;

use crate::actor::{
    context::ActorContext,
    raw::{ActorResult, RawActor},
};

/// How the provided [`Actor`] receive loop treats messages still
/// queued when shutdown is requested.
///
/// # Startup ordering and concurrent shutdown
///
/// Actors initialize concurrently by default. With
/// [`StartMode::Sequential`](tokio_supervisor::StartMode::Sequential), each
/// actor's [`on_start`](Actor::on_start) must finish before the next actor is
/// spawned. At shutdown, every sibling is cancelled at the same time under
/// one shared grace deadline. A draining actor can therefore
/// observe a [`SendError`](crate::SendError) from a sibling that has already
/// stopped; drain handlers must tolerate that (skip or log the failed send)
/// rather than propagate it, or the error fails the draining actor itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum DrainPolicy {
    /// Stop immediately.
    ///
    /// Queued messages are dropped and queued [`call`](crate::ActorRef::call)s
    /// observe [`ReplyDropped`](crate::CallError::ReplyDropped). This matches
    /// the behavior of a hand-written `while let Some(message) =
    /// ctx.recv().await` loop.
    #[default]
    Discard,
    /// Close the mailbox to new sends, handle every message already queued,
    /// then stop.
    ///
    /// The drain is not separately time-bounded; the surrounding shutdown
    /// backstop applies:
    /// [`GraphBuilder::actor_shutdown_timeout`](crate::GraphBuilder::actor_shutdown_timeout)
    /// for the actor run itself, and the supervisor child shutdown policy
    /// when the actor is hosted under `tokio-supervisor`.
    Drain,
}

/// Handler-style actor interface with a framework-owned receive loop.
///
/// Implement this trait when an actor only needs one method per message. The
/// blanket [`RawActor`] implementation receives messages in mailbox order, runs
/// lifecycle hooks, and applies [`DrainPolicy`] at shutdown.
///
/// Hand-writing [`RawActor::run`] remains the escape hatch for actors that need
/// custom loop control.
///
/// # Incarnation construction
///
/// Registration takes a reusable [`ActorFactory`](crate::ActorFactory), whose
/// output is fresh incarnation-local state and need not implement [`Clone`].
/// Acquire fallible or asynchronous per-incarnation resources (connections,
/// files, sessions) in [`on_start`](Self::on_start).
pub trait Actor: Send + Sync + 'static {
    /// The message type this handler receives.
    type Msg: Send + 'static;

    /// Handles one received message.
    ///
    /// Returning `Err` fails the actor exactly like [`RawActor::run`] returning
    /// `Err`.
    fn handle(
        &mut self,
        message: Self::Msg,
        ctx: &ActorContext<Self::Msg>,
    ) -> impl Future<Output = ActorResult> + Send;

    /// Runs once before the first message of each actor run.
    ///
    /// This is the place to acquire per-incarnation resources; an error here
    /// fails the run like a [`handle`](Self::handle) error, so under
    /// supervision it is an ordinary restartable failure.
    fn on_start(
        &mut self,
        _ctx: &ActorContext<Self::Msg>,
    ) -> impl Future<Output = ActorResult> + Send {
        async { Ok(()) }
    }

    /// Runs once after the receive loop exits cleanly.
    ///
    /// This hook also runs after a drain. It is not called when
    /// [`handle`](Self::handle) or [`on_start`](Self::on_start) returns an
    /// error.
    fn on_stop(
        &mut self,
        _ctx: &ActorContext<Self::Msg>,
    ) -> impl Future<Output = ActorResult> + Send {
        async { Ok(()) }
    }

    /// Returns this handler's shutdown drain policy.
    fn drain_policy(&self) -> DrainPolicy {
        DrainPolicy::Discard
    }
}

impl<H: Actor> RawActor for H {
    type Msg = H::Msg;

    fn readiness_gated(&self) -> bool {
        true
    }

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        self.on_start(&ctx).await?;
        ctx.mark_ready();

        loop {
            let message = if let Some(message) = ctx.take_continuation() {
                Some(message)
            } else {
                tokio::select! {
                    biased;
                    _ = ctx.shutdown.cancelled() => {
                        ctx.close_external_intake();
                        if self.drain_policy() == DrainPolicy::Drain {
                            // Waiting for every step before receiving would
                            // deadlock when a full FIFO mailbox backpressures
                            // a completion. Close external intake, then drain
                            // messages and incarnation-local step completions
                            // together until both sources are quiescent.
                            loop {
                                let message = if let Some(message) = ctx.take_continuation() {
                                    Some(message)
                                } else {
                                    match ctx.mailbox.try_recv() {
                                        Ok(message) => Some(message),
                                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                            None
                                        }
                                        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                                            if ctx.outstanding_steps() == 0 =>
                                        {
                                            // A final step can enqueue its postback after the
                                            // first poll and then deregister before the gauge
                                            // check. Deregistration synchronizes through the
                                            // step registry, so re-poll after observing zero
                                            // before declaring the mailbox quiescent.
                                            ctx.mailbox.try_recv().ok()
                                        }
                                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                            let changed = ctx.step_change_notify();
                                            tokio::select! {
                                                message = ctx.mailbox.recv() => message,
                                                () = changed.notified() => continue,
                                            }
                                        }
                                    }
                                };
                                let Some(message) = message else { break };
                                ctx.myself.record_received();
                                ctx.observability.emit_message_received(&ctx.id);
                                self.handle(message, &ctx).await?;
                            }
                        } else {
                            ctx.abort_steps();
                        }
                        break;
                    }
                    message = ctx.mailbox.recv() => message,
                }
            };

            let Some(message) = message else {
                break;
            };
            ctx.myself.record_received();
            ctx.observability.emit_message_received(&ctx.id);
            self.handle(message, &ctx).await?;
        }

        self.on_stop(&ctx).await
    }
}
