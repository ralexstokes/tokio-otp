# Bounded actor steps

An actor that awaits slow work inside `handle` stops receiving messages until
that work completes. `ActorContext::step` moves a bounded future off the
handler loop and maps its result back into an ordinary typed message:

```rust,no_run
use std::time::Duration;
use tokio_otp::{ActorContext, StepDeadline};

enum Msg {
    Loaded(Result<String, StepDeadline>),
}

# async fn load() -> String { String::new() }
# fn start(ctx: &ActorContext<Msg>) {
ctx.step(Duration::from_millis(250), load(), Msg::Loaded);
# }
```

The deadline is required, and the continuation is total: it receives
`Result<T, StepDeadline>` so timeout cannot be forgotten. The completion uses
the actor's ordinary mailbox policy. A full FIFO mailbox backpressures it;
conflating mailboxes may replace it like any other message.

## Incarnations and correlation

Every postback is stamped to the incarnation that started it. If that
incarnation fails or restarts, the future is aborted and a racing completion
is silently dropped instead of following the restart-stable `ActorRef` into
fresh in-memory state.

The library owns only this cross-incarnation staleness rule. Correlation among
concurrent steps in one incarnation remains part of the message protocol:

```rust,no_run
# use std::time::Duration;
# use tokio_otp::{ActorContext, StepDeadline};
enum Msg {
    Fetched { request: u64, value: Result<String, StepDeadline> },
}
# async fn fetch() -> String { String::new() }
# fn start(ctx: &ActorContext<Msg>, request: u64) {
ctx.step(Duration::from_secs(1), fetch(), move |value| {
    Msg::Fetched { request, value }
});
# }
```

The request id is still necessary when multiple fetches can overlap. A hand
rolled incarnation or turn tag used only to reject results after restart is
not.

## Abort is not undo

`StepHandle::abort`, timeout, actor failure, and discard shutdown all abandon
the local future. They cannot retract a request another actor or external
service already accepted. Its outcome is unknown, not "not executed."

Step futures should therefore initiate requests rather than mutate untracked
local state directly. Put effects behind actors and protect retryable commands
with idempotency keys and reconciliation. Domain cancellation remains
explicit: capture a `CancellationToken` in the future when the remote protocol
supports it.

## Shutdown

Steps follow handler actors' `DrainPolicy`:

- `Discard` closes external intake and aborts outstanding steps as soon as
  shutdown reaches the actor loop.
- `Drain` closes external intake, then processes queued messages and step
  completions until both are exhausted. A drained message may start another
  step, which joins the same bounded drain.

Draining must interleave messages and completions. Waiting for all steps first
would deadlock when a full FIFO mailbox backpressures a completion that needs
the actor to receive one queued message before capacity becomes available.
Each step future is still bounded by its own required deadline; the actor and
supervisor shutdown timeouts remain the outer backstops for slow handlers.

`ActorStats::outstanding_steps` exposes the current number of owned steps.
The method lives on the shared `ActorContext` type, but automatic shutdown and
drain integration belongs to the framework-owned `Actor` loop. A `RawActor`
that uses `step` must define its own receive and shutdown protocol.
