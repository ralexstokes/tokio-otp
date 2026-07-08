# tokio-otp

Erlang/OTP-style fault tolerance for the [`tokio`](https://tokio.rs) ecosystem:
supervision trees, typed actor graphs, and a runtime that composes the two.

The core idea is the one that has kept telecom switches running for decades:
**let it crash**. Instead of defensively handling every failure in place, you
organize your program into small, isolated tasks and let a *supervisor*
restart the ones that fail.

One dependency is the front door — `tokio-otp` re-exports everything you
need through its prelude:

```toml
[dependencies]
tokio-otp = { git = "https://github.com/ralexstokes/tokio-otp" }
```

## A taste

Each actor runs as its own supervised child. When the press crashes, the
supervisor restarts it — and the `orders` ref keeps working across the
restart, transparently reconnecting to the replacement:

```rust
use tokio_otp::prelude::*;

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
    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .build()?;
    let handle = runtime.spawn();

    orders.send_when_ready("business cards x100".to_owned()).await?;

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

The full runnable version is
[`crates/tokio-otp/examples/supervised_actors.rs`](crates/tokio-otp/examples/supervised_actors.rs).

## The crates, à la carte

`tokio-otp` is a thin composition layer over two deliberately independent
crates: `tokio-supervisor` knows nothing about actors, and `tokio-actor`
knows nothing about restarts. Each is useful on its own — depend on one
directly if you only need that piece.

| Crate | Role |
|-------|------|
| [`tokio-otp`](crates/tokio-otp) | The front door: run each actor of a graph as its own supervised child, with one integrated `Runtime` supporting dynamic actors and observability. Re-exports the common types of the crates below via `tokio_otp::prelude`. |
| [`tokio-supervisor`](crates/tokio-supervisor) | Structured supervision of async tasks: restart policies (`permanent`/`transient`/`temporary`), restart intensity limits, `one_for_one`/`one_for_all` strategies, graceful shutdown, and nested supervision trees. |
| [`tokio-actor`](crates/tokio-actor) | Static graphs of communicating actors: typed mailboxes, restart-stable `ActorRef<M>` handles, request/reply, and blocking-task integration. |
| [`tokio-otp-console`](crates/tokio-otp-console) | *(experimental)* A live web dashboard for watching a running supervision tree. |

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
to crates.io — use a git dependency as shown above.

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
