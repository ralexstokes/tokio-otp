# Observability

The crates expose four views into a running system:

1. supervisor events
2. supervisor snapshots
3. `tracing` and optional `metrics`
4. the `tokio-otp-console` web UI

## Events And Snapshots

`RuntimeHandle::subscribe()` returns supervisor lifecycle events. Use it to
react to restarts or removals:

```rust,ignore
let mut events = handle.subscribe();
tokio::spawn(async move {
    while let Ok(event) = events.recv().await {
        println!("event: {event:?}");
    }
});
```

`RuntimeHandle::snapshot()` returns the current tree state, and
`subscribe_snapshots()` returns a `watch::Receiver` that updates when the
snapshot changes.

## Tracing And Metrics

`tokio-actor` emits graph, actor, mailbox, message, and blocking-task tracing
events. Message events include `source_actor_id` when the sender is another
actor; external sends through an `ActorRef` have no source actor.

The old external-entry and byte-size fields are gone. Typed messages do not have a
crate-level byte length, so input sizing belongs at the application boundary.

With the `metrics` feature, actor metrics include:

- `actor_graph.runs.started` / `actor_graph.runs.stopped`
- `actor_graph.actors.started` / `actor_graph.actors.exited`
- `actor_graph.messages.sent` / `actor_graph.messages.rejected`
- `actor_graph.messages.received`
- `actor_graph.mailbox.bound`
- `actor_graph.blocking.started` / `actor_graph.blocking.completed`
- duration histograms for graph runs, sends, and blocking tasks

`tokio-supervisor` continues to emit supervisor lifecycle counters, gauges,
and histograms.

## Web Console

With the `console` feature, a runtime handle can launch a web console backed
by the same snapshots and events:

```rust,ignore
let handle = runtime.spawn();
let console = handle
    .console()
    .bind(([127, 0, 0, 1], 8080))
    .build()
    .spawn()
    .await?;

println!("console at http://{}", console.local_addr());
```

Run `cargo run -p tokio-otp --example console --features console` to try it.
