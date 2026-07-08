# tokio-otp

Erlang/OTP-style fault tolerance for the [`tokio`](https://tokio.rs) ecosystem:
supervision trees, typed actor graphs, and a runtime that composes the two.

The core idea is the one that has kept telecom switches running for decades:
**let it crash**. Instead of defensively handling every failure in place, you
organize your program into small, isolated tasks and let a *supervisor*
restart the ones that fail.

## Crates

| Crate | Role |
|-------|------|
| [`tokio-supervisor`](crates/tokio-supervisor) | Structured supervision of async tasks: restart policies (`permanent`/`transient`/`temporary`), restart intensity limits, `one_for_one`/`one_for_all` strategies, graceful shutdown, and nested supervision trees. |
| [`tokio-actor`](crates/tokio-actor) | Static graphs of communicating actors: typed mailboxes, restart-stable `ActorRef<M>` handles, request/reply, and blocking-task integration. |
| [`tokio-otp`](crates/tokio-otp) | The composition layer: run each actor of a graph as its own supervised child, with one integrated `Runtime` supporting dynamic actors and observability. |
| [`tokio-otp-console`](crates/tokio-otp-console) | *(experimental)* A live web dashboard for watching a running supervision tree. |

The crates are deliberately independent: `tokio-supervisor` knows nothing
about actors, `tokio-actor` knows nothing about restarts, and `tokio-otp`
glues them together without boilerplate.

## A taste

Each actor runs as its own supervised child. When the press crashes, the
supervisor restarts it — and the `orders` ref keeps working across the
restart, transparently reconnecting to the replacement:

```rust
use tokio_actor::{Actor, ActorContext, ActorRef, ActorResult, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{Restart, Strategy, SupervisorBuilder};

#[derive(Clone)]
struct FrontDesk {
    press: ActorRef<String>,
}

impl Actor for FrontDesk {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(order) = ctx.recv().await {
            let mut press = self.press.clone();
            press.send_when_ready(order).await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Wire a static graph with typed, restart-stable actor refs.
    let mut builder = GraphBuilder::new();
    let press_ref = builder.declare::<String>("press");
    let mut orders = builder.actor("front-desk", FrontDesk { press: press_ref });
    builder.actor("press", Press::default()); // an actor that occasionally jams
    let graph = builder.build()?;

    // Run every actor as its own supervised child.
    let runtime = SupervisedActors::new(graph)?
        .restart(Restart::Transient)
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    orders.wait_for_binding().await;
    orders.send("business cards x100".to_owned()).await?;

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

The full runnable version is
[`crates/tokio-otp/examples/supervised_actors.rs`](crates/tokio-otp/examples/supervised_actors.rs).

## Getting started

- **Tutorial book** — builds a small fault-tolerant service from scratch,
  from supervision fundamentals through dynamic actors and observability.
  Start at [`docs/src/introduction.md`](docs/src/introduction.md), or run
  `just serve-book` for a local copy.
- **API docs** — `just doc` builds and opens the rustdoc for the workspace.
- **Examples** — each crate ships runnable examples under its `examples/`
  directory, e.g. `cargo run -p tokio-otp --example supervised_actors`.

## Status

Early-stage and evolving; APIs may change. The crates are not yet published
to crates.io — use a git dependency:

```toml
[dependencies]
tokio-otp = { git = "https://github.com/ralexstokes/tokio-otp" }
```

## Development

The Nix flake provides both local tooling and CI:

```sh
nix develop
just ci      # fast local mirror of CI (fmt, clippy, build, tests, book)
just ci-nix  # exactly what GitHub Actions runs; must pass before pushing
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
