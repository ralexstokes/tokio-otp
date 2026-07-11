# Actor monitors

An actor can monitor another actor when a request or piece of state depends on
that peer staying alive. A monitor turns the peer's exit into an ordinary typed
message in the observer's mailbox:

```rust
use tokio_otp::{Actor, ActorContext, ActorRef, ActorResult, Down};

enum CoordinatorMsg {
    WorkerDown(Down),
}

#[derive(Clone)]
struct Coordinator {
    worker: ActorRef<String>,
}

impl Actor for Coordinator {
    type Msg = CoordinatorMsg;

    async fn on_start(&mut self, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        ctx.monitor(&self.worker, CoordinatorMsg::WorkerDown);
        Ok(())
    }

    async fn handle(
        &mut self,
        message: Self::Msg,
        _ctx: &ActorContext<Self::Msg>,
    ) -> ActorResult {
        let CoordinatorMsg::WorkerDown(down) = message;
        eprintln!(
            "{} generation {} exited: {:?}",
            down.actor_id, down.generation, down.reason
        );
        Ok(())
    }
}
```

`DownReason::Normal` covers a clean return and cooperative shutdown.
`DownReason::Failure` covers errors, panics, aborts, and a dropped actor run.
`DownReason::NoProcess` is delivered immediately when the ref is already
exited, terminated, or detached. For a never-started actor, the generation in
a `NoProcess` notification is only a placeholder.

A monitor targets one incarnation. The first actor run is generation `0`, and
each restart increments the generation. After receiving `Down`, register a new
monitor if the observer also needs to watch the replacement incarnation. A
monitor registered before the target's first start waits for generation `0`,
which removes actor-start ordering races from a graph.

`ActorContext::monitor` returns a cloneable `MonitorRef`. Calling `cancel` on
any clone suppresses future delivery. Cancellation cannot retract a `Down`
message already accepted by the mailbox. All monitors are also cancelled when
the observing actor stops or restarts, so an old observer incarnation cannot
inject messages into its replacement.

Monitor notifications use the observer's ordinary mailbox. FIFO mailboxes wait
for capacity and preserve every accepted notification. A conflating mailbox
may replace an unread monitor message, so use FIFO when every peer exit must be
observed.
