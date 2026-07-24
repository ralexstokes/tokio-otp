# Bounded Request/Reply

`ActorRef::call` builds request/reply on the ordinary actor mailbox: it creates
a one-shot `Reply<T>`, puts that reply handle in your message, sends the
message, and waits for the actor to answer. Deadlines are caller-owned, so
bound the whole operation with `tokio::time::timeout`:

```rust,no_run
use std::time::Duration;
use tokio::time::timeout;
use tokio_otp::{ActorRef, Reply};

enum AccountMsg {
    Balance(Reply<u64>),
}

async fn balance(
    account: &ActorRef<AccountMsg>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let balance = timeout(
        Duration::from_millis(250),
        account.call(AccountMsg::Balance),
    )
    .await??;
    Ok(balance)
}
```

The timeout covers both phases of a call:

1. **Delivery:** `call` waits for the same conditions as `send`. Before the
   actor starts, or during an expected restart, it waits for a mailbox to bind.
   With a full FIFO mailbox, it waits for capacity. If the deadline expires in
   this phase, cancelling the future drops the request before acceptance.
2. **Reply:** after the mailbox accepts the request, `call` waits on its
   one-shot reply channel. If the deadline expires now, only the caller's wait
   is cancelled. The accepted request remains in the mailbox, the actor may
   still process it, and `Reply::send` silently discards a late result.

This boundary means that a timed-out call can have an **unknown outcome**. The
caller cannot generally tell whether the request never reached the actor, is
still queued, is currently running, or completed after the deadline.

## Side effects and retries

For read-only operations, ignoring a late reply is often enough. For commands
that write to a database, charge a card, publish an event, or otherwise affect
an external system, do not treat a timeout as proof that nothing happened.
Give each logical operation an idempotency key that the actor and external
system persist, or provide a reconciliation query that lets the caller learn
the final status. Retry with the same key rather than creating a second
logical operation.

Cancellation of the `call` future is not a cancellation signal for actor work.
If a protocol needs cooperative cancellation, model it explicitly in the
message type and define what happens when cancellation races with completion.

## Backpressure and restarts

The composed timeout deliberately includes mailbox backpressure and restart
backoff. Choose a deadline that covers the queueing delay your service is
willing to tolerate:

- Use `try_send` for fire-and-forget messages when failing fast on a full
  mailbox beats waiting. There is no fail-fast variant of `call`.
- Use `timeout(..., actor.call(...))` when the caller can wait for capacity or
  a short restart window, but needs a firm end-to-end deadline.
- Do not use `call` with a conflating mailbox. A newer value can replace the
  request, causing `CallError::ReplyDropped`.

A call waiting during an expected restart can succeed after the new actor
incarnation binds. A request accepted by the old incarnation before it stops
is still subject to the at-most-once delivery contract: it may be lost with
that incarnation, in which case the reply channel closes with
`CallError::ReplyDropped`.

## Head-of-line blocking: calls from inside a handler

An actor processes one message at a time. When a handler awaits a `call` to
another actor, the calling actor's mailbox stops for the full round-trip:
every queued message — a cancel bound for a healthy peer, an urgent status
query — waits behind the outstanding request for up to the composed
deadline. This is the natural way to write a request-routing actor, and it
is the actor-model equivalent of blocking inside an Erlang `gen_server`
callback: one slow callee becomes head-of-line blocking for everything
routed through the intermediary.

For fan-out and routing actors, pipeline the request instead of awaiting
it. The handler validates and records intent, then spawns the bounded call
onto a task. The task completes the original caller's `Reply` and sends the
outcome back to the actor as an ordinary message, so state updates still
happen on the actor's serial handle loop:

```rust,no_run
use std::{collections::HashMap, time::Duration};
use tokio::time::timeout;
use tokio_otp::prelude::*;

enum VenueMsg {
    Place { order: u64, reply: Reply<bool> },
}

enum RouterMsg {
    Submit {
        venue: &'static str,
        order: u64,
        reply: Reply<bool>,
    },
    // Internal: the pipelined call's outcome, sent by the spawned task.
    Resolved {
        order: u64,
        accepted: bool,
    },
}

struct Router {
    venues: HashMap<&'static str, ActorRef<VenueMsg>>,
    in_flight: HashMap<u64, &'static str>,
}

impl Actor for Router {
    type Msg = RouterMsg;

    async fn handle(
        &mut self,
        message: RouterMsg,
        ctx: &ActorContext<RouterMsg>,
    ) -> ActorResult {
        match message {
            RouterMsg::Submit { venue, order, reply } => {
                // Validate and record intent on the handle loop...
                self.in_flight.insert(order, venue);
                let gateway = self.venues[venue].clone();
                let myself = ctx.myself();
                // ...then move the slow call off it.
                tokio::spawn(async move {
                    let accepted = matches!(
                        timeout(
                            Duration::from_millis(250),
                            gateway.call(|reply| VenueMsg::Place { order, reply }),
                        )
                        .await,
                        Ok(Ok(true))
                    );
                    // Update the router first, then release the caller, so a
                    // follow-up message is ordered after the resolution.
                    let _ = myself.send(RouterMsg::Resolved { order, accepted }).await;
                    reply.send(accepted);
                });
            }
            RouterMsg::Resolved { order, accepted } => {
                // Back on the handle loop: apply the outcome to actor state.
                self.in_flight.remove(&order);
                if !accepted {
                    // schedule reconciliation, raise an alert, ...
                }
            }
        }
        Ok(tokio_otp::prelude::Continue)
    }
}
```

Two properties make the pattern work:

- **Reply ownership.** `Reply` is a one-shot handle that can move into the
  spawned task, so the original caller is answered without another pass
  through the router.
- **Mailbox ordering.** The task sends the resolution message and waits for
  the mailbox to accept it *before* completing the caller's reply. Anything
  the caller sends after seeing the reply is queued behind the resolution in
  the actor's FIFO mailbox, so within a single actor incarnation a
  follow-up is never processed before the outcome it follows.

The spawned task is not bound to the actor's lifecycle, and the ordering
above is a same-incarnation guarantee. If the actor restarts while a call
is in flight, the task keeps running, but its resolution message is subject
to the at-most-once contract described above: accepted by the old
incarnation's mailbox and not yet handled, it is lost with that
incarnation; sent after the restart, it is delivered to the new
incarnation, which has no matching in-flight state. Two rules keep this
safe. Correlation keys must be unique across incarnations — drawn from
restart-stable state, not from a counter that resets with the actor — so a
stale resolution can never alias a new request's state and an unmatched one
can simply be dropped. And an outcome that must survive the actor needs
durable state or a reconciliation path; otherwise a lost resolution leaves
the request unknown forever. When per-callee state outgrows what a
resolution message can carry, promote the callee to a dedicated child actor
and let supervision manage its lifecycle instead.

Not every handler needs this treatment. A serial batch operation that
mutates actor state between calls — a reconciliation sweep run while
intake is quiet, for example — can reasonably stay inline, provided
blocking the mailbox for its duration is an explicit, accepted trade-off.

The `trading_engine` example's order router demonstrates the full pattern,
including a phase that proves an order for a healthy venue completes while
another venue's call is still waiting out its deadline.
