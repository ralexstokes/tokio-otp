# Getting started

## Dependencies

The crates are not yet published to crates.io, so use a git dependency (or a
path dependency if you are working inside this repository). One dependency is
enough for the whole tutorial:

```toml
[dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time"] }
tokio-otp = { git = "https://github.com/ralexstokes/tokio-otp" }
```

`tokio-otp` re-exports the common types of its underlying crates through
`tokio_otp::prelude`, so a single `use tokio_otp::prelude::*;` covers every
example in this book. The lower-level crates are also usable à la carte: the
early chapters only exercise the supervision layer, so if that is all you
need, you can depend on `tokio-supervisor` directly (and likewise
`tokio-actor` for unsupervised actor graphs).

## Your first supervised task

Before the print shop opens for business, let's supervise the simplest thing
possible: a heartbeat task. A supervisor is built from one or more
[`ChildSpec`]s. Each child spec pairs an *async factory* — a closure the
supervisor calls every time it needs to (re)start the child — with restart and
shutdown policies.

```rust,no_run
use std::time::Duration;

use tokio_otp::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("heartbeat", |ctx| async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = ctx.token.cancelled() => {
                        println!("heartbeat asked to stop");
                        return Ok(());
                    }
                    _ = ticker.tick() => {
                        println!("beat (generation {})", ctx.generation);
                    }
                }
            }
        }))
        .build()?;

    let handle = supervisor.spawn();

    tokio::time::sleep(Duration::from_secs(2)).await;

    let exit = handle.shutdown_and_wait().await?;
    println!("supervisor exited with {exit:?}");
    Ok(())
}
```

A few things worth noticing:

- **The factory receives a [`ChildContext`]** (`ctx`). It carries the child's
  `id`, its `generation` (0 for the first spawn, incremented on every
  restart), and a `token` — a `CancellationToken` the supervisor cancels when
  the child should stop. Well-behaved children select on it.
- **The child returns `Result<(), BoxError>`.** Returning `Ok(())` is a clean
  exit; returning an `Err`, panicking, or being aborted counts as a failure.
  The restart policy decides what happens next.
- **`spawn()` returns a [`SupervisorHandle`].** This is your control surface:
  shut the tree down, add or remove children, subscribe to lifecycle events,
  or grab a state snapshot. `run()` is the alternative if you'd rather drive
  the supervisor on the current task.

Run it and you'll see the heartbeat tick until the shutdown request cancels
its token:

```text
beat (generation 0)
beat (generation 0)
beat (generation 0)
beat (generation 0)
heartbeat asked to stop
supervisor exited with Shutdown
```

So far the child never fails, so the supervisor has nothing interesting to do.
Let's fix that.

[`ChildSpec`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
[`ChildContext`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
[`SupervisorHandle`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
