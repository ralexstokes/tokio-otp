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
FIFO delivery waits for capacity just like `ActorRef::send`; conflating
mailboxes replace stale unread state instead. Intervals do not build
an unbounded backlog while waiting: missed ticks are skipped. A one-shot timer
that has fired is complete, not cancelled, so its ref still reports
`is_cancelled() == false`.

Timers belong to one actor incarnation. The runtime cancels all of them when
that incarnation stops, restarts, or observes shutdown. A timer task waiting
for mailbox capacity is cancelled too, so it cannot follow the actor's stable
ref and leak a stale message into the next incarnation.

## State timeouts

Rust enums already make actor state machines explicit. `state_timeout` adds
the one piece that otherwise requires bookkeeping: a single timeout slot that
cancels and replaces the previous timeout whenever the actor enters another
timed state. `clear_state_timeout` cancels the slot when entering an untimed
state.

```rust,ignore
use std::time::Duration;

use tokio_otp::prelude::*;

#[derive(Clone)]
enum Message {
    Filled,
    FillTimedOut,
    Cancelled,
}

#[derive(Clone, Default)]
enum Order {
    #[default]
    PendingFill,
    Cancelling,
    Complete,
}

impl Actor for Order {
    type Msg = Message;

    async fn on_start(&mut self, ctx: &ActorContext<Message>) -> ActorResult {
        ctx.state_timeout(Message::FillTimedOut, Duration::from_millis(500));
        Ok(())
    }

    async fn handle(&mut self, message: Message, ctx: &ActorContext<Message>) -> ActorResult {
        match (&*self, message) {
            (Order::PendingFill, Message::Filled) => {
                ctx.clear_state_timeout();
                *self = Order::Complete;
            }
            (Order::PendingFill, Message::FillTimedOut) => {
                *self = Order::Cancelling;
                ctx.state_timeout(Message::Cancelled, Duration::from_secs(2));
            }
            (Order::Cancelling, Message::Cancelled) => {
                ctx.clear_state_timeout();
                *self = Order::Complete;
            }
            _ => {}
        }
        Ok(())
    }
}
```

Calling `state_timeout` owns the message and returns a `TimerRef`, like
`send_after`, but the actor does not need to retain it. Entering another timed
state and calling `state_timeout` automatically cancels the previous timeout.
Call `clear_state_timeout` when entering a state without a timeout.

Timeout messages that were already accepted by the mailbox cannot be
retracted. Each timeout message must therefore identify the state it was set
for, either with a distinct variant as above or with a state or generation
tag, and the handler must reject tags that do not match its current state.
Replacing or clearing a state timeout cancels its token even if it already
fired, so a retained handle will then report `is_cancelled() == true`. State
timeouts are also cleared with all other timers when the actor stops or
restarts.
