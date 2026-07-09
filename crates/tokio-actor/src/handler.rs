use std::future::Future;

use crate::{
    actor::{ActorResult, RawActor},
    context::ActorContext,
};

/// How the provided [`Actor`] receive loop treats messages still
/// queued when graph shutdown is requested.
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
    /// The drain is bounded by the same shutdown backstops as any actor run:
    /// [`GraphBuilder::actor_shutdown_timeout`](crate::GraphBuilder::actor_shutdown_timeout)
    /// in graph-run mode, or the supervisor child shutdown policy when a
    /// runnable actor is hosted under `tokio-supervisor`.
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
pub trait Actor: Clone + Send + Sync + 'static {
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

    async fn run(&self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let mut handler = self.clone();
        handler.on_start(&ctx).await?;

        loop {
            let message = tokio::select! {
                biased;
                _ = ctx.shutdown.cancelled() => {
                    if handler.drain_policy() == DrainPolicy::Drain {
                        ctx.mailbox.close();
                        while let Some(message) = ctx.mailbox.recv().await {
                            ctx.observability.emit_message_received(&ctx.id);
                            handler.handle(message, &ctx).await?;
                        }
                    }
                    break;
                }
                message = ctx.mailbox.recv() => message,
            };

            let Some(message) = message else {
                break;
            };
            ctx.observability.emit_message_received(&ctx.id);
            handler.handle(message, &ctx).await?;
        }

        handler.on_stop(&ctx).await
    }
}
