# Observability

The crates expose four views into a running system:

1. supervisor events
2. supervisor snapshots
3. `tracing`, pull-based actor stats, and optional metrics
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

## Tracing And Stats

The actor layer emits graph, actor, mailbox, and message tracing events.
Message events include `source_actor_id` when the sender is another actor;
external sends through an `ActorRef` have no source actor.

Every `ActorRef` exposes cumulative message counters and current mailbox usage:

```rust,ignore
let stats = worker.stats();
println!("received={} queued={}/{}",
    stats.messages_received, stats.mailbox_depth, stats.mailbox_capacity);
```

Applications that need time-series export periodically sample these values and
the supervisor snapshot — a ~10-line task you own, not a framework pipeline.
The `tokio-otp` `actor_metrics` example prints the result in
Prometheus-shaped text without an actor-layer metrics backend.

Message sizes are application-defined and fully opt-in. Implement
`MessageSize` for a message type and register its actor with
`actor_with_message_size` (or a cyclic slot with `slot_with_message_size`):

```rust,ignore
impl MessageSize for Upload {
    fn size_hint(&self) -> usize {
        self.payload.len()
    }
}

let uploads = graph.actor_with_message_size("uploads", UploadActor::new());
```

For runtime-added actors, use
`RuntimeHandle::add_actor_with_message_size`; lower-level dynamic factories
provide the corresponding `RunnableActorFactory::actor_with_message_size`.

`ActorStats::message_bytes_accepted` is then `Some(total)`; ordinary actors
report `None` and do not sample message sizes. With the `metrics` feature,
each accepted sized message also updates the `actor.message.size` histogram
and `actor.message.bytes_accepted` counter. Metric handles and actor-id labels
are registered lazily on the first accepted message and cached per actor; later
accepted sends only sample `size_hint` and update those handles. The feature
continues to enable the supervisor lifecycle counters, gauges, and histograms
as well.

## Web Console

With the `console` feature, a runtime handle can launch a web console backed
by the same snapshots, events, and actor stats:

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
