# Introduction

`tokio-otp` is a small family of crates that bring Erlang/OTP-style fault
tolerance to the [`tokio`](https://tokio.rs) ecosystem. The core idea is the
same one that has kept telecom switches running for decades: **let it crash**.
Instead of writing defensive code that tries to recover from every possible
failure in place, you organize your program into small, isolated tasks and let
a *supervisor* restart the ones that fail.

## The crates

| Crate | Role |
|-------|------|
| [`tokio-supervisor`](https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor) | Structured supervision of async tasks: restart policies, restart intensity limits, graceful shutdown, and supervision trees. |
| [`tokio-actor`](https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor) | Static graphs of communicating actors: typed mailboxes, restart-stable `ActorRef<M>` handles, request/reply, and blocking-task integration. |
| [`tokio-otp`](https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp) | The composition layer: run each actor of a graph as its own supervised child, with one integrated `Runtime`. |
| [`tokio-otp-console`](https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp-console) | A live web console for watching a running supervision tree. |

The crates are deliberately independent:

- `tokio-supervisor` knows nothing about actors. It supervises any async task.
- `tokio-actor` knows nothing about restarts. When an actor fails, the whole
  graph run fails — which makes a graph a perfect *child* for a supervisor.
- `tokio-otp` glues the two together and removes the boilerplate.

## The mental model

If you have used Erlang/OTP or Elixir, the mapping is direct:

| OTP concept | tokio-otp equivalent |
|-------------|----------------------|
| Supervisor + child specs | [`SupervisorBuilder`] + [`ChildSpec`] |
| `one_for_one` / `one_for_all` | `Strategy::OneForOne` / `Strategy::OneForAll` |
| `permanent` / `transient` / `temporary` | `Restart::Permanent` / `Restart::Transient` / `Restart::Temporary` |
| Restart intensity (`MaxR`/`MaxT`) | `RestartIntensity::new(max_restarts, within)` |
| GenServer-ish process with a mailbox | An actor with an [`ActorContext`] |
| Registered process name | Actor id + typed `ActorRef<M>` |

If you have not: don't worry. This tutorial builds everything up from scratch.

## The running example

Throughout the tutorial we build a tiny **print shop** service:

```text
                 ┌────────────┐      ┌───────┐      ┌──────────┐
  orders ref ──▶ │ front-desk │ ───▶ │ press │ ───▶ │ shipping │
                 └────────────┘      └───────┘      └──────────┘
```

- Customers submit print orders through a typed `ActorRef<Order>` (the stable
  entry point into the graph).
- The **front-desk** actor validates orders and forwards them.
- The **press** actor does the actual printing — and occasionally jams.
- The **shipping** actor records finished jobs.

The press jamming is the interesting part: we want the rest of the shop to
keep running while a supervisor replaces the press, and we want in-flight
senders to transparently reconnect to the new press. That is exactly what
these crates are for.

## How to read this tutorial

Each chapter is a complete, runnable program. You can paste any of them into a
binary crate and run it, or explore the closely related examples that ship in
each crate's `examples/` directory (listed in [Where to go
next](next-steps.md)).

[`SupervisorBuilder`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
[`ChildSpec`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
[`ActorContext`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
