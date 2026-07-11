# Actor Timers

Actors often need to schedule work for themselves: retry a connection after a
delay, expire an order, send a heartbeat, or reconcile state periodically.
`ActorContext` turns those events into ordinary typed mailbox messages, so the
same `Actor::handle` method handles both external and timed work.

```rust,ignore
use std::time::Duration;

use tokio_otp::prelude::*;

#[derive(Clone)]
enum Message {
    Reconnect,
    Reconcile,
}

#[derive(Clone, Default)]
struct Worker {
    reconnect: Option<TimerRef>,
    reconcile: Option<TimerRef>,
}

impl Actor for Worker {
    type Msg = Message;

    async fn on_start(&mut self, ctx: &ActorContext<Message>) -> ActorResult {
        self.reconnect = Some(ctx.send_after(Message::Reconnect, Duration::from_secs(5)));
        self.reconcile = Some(ctx.interval(Message::Reconcile, Duration::from_secs(30)));
        Ok(())
    }

    async fn handle(&mut self, message: Message, _ctx: &ActorContext<Message>) -> ActorResult {
        match message {
            Message::Reconnect => { /* reconnect once */ }
            Message::Reconcile => { /* reconcile periodically */ }
        }
        Ok(())
    }
}
```

`send_after` owns its message and sends it once after the delay. `interval`
clones its message on each period, so its message type must implement `Clone`.
Both return a cloneable `TimerRef`; calling `cancel` on any clone cancels the
same timer. Dropping a timer ref does not cancel it.

Timer messages use the normal bounded mailbox. When the mailbox is full,
delivery waits for capacity just like `ActorRef::send`. Intervals do not build
an unbounded backlog while waiting: missed ticks are skipped. A one-shot timer
that has fired is complete, not cancelled, so its ref still reports
`is_cancelled() == false`.

Timers belong to one actor incarnation. The runtime cancels all of them when
that incarnation stops, restarts, or observes shutdown. A timer task waiting
for mailbox capacity is cancelled too, so it cannot follow the actor's stable
ref and leak a stale message into the next incarnation.
