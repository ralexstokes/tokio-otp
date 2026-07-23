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
`MessageSize` for a message type and enable it in the actor's `ActorOptions`:

```rust,ignore
impl MessageSize for Upload {
    fn size_hint(&self) -> usize {
        self.payload.len()
    }
}

let uploads = graph.actor_with_options(
    "uploads",
    UploadActor::new,
    ActorOptions::new().message_size(),
);
```

The same options value works with `GraphBuilder::slot_with_options`,
`GraphBuilder::add_with_options`, `RunnableActorFactory::actor_with_options`,
and `RuntimeHandle::add_actor_with_options`. Mailbox and size settings can be
combined, for example with
`ActorOptions::new().mailbox(MailboxMode::Conflate).message_size()`.

`RuntimeHandle::actor_stats()` walks runtime subtrees recursively. A handle
returned by `RuntimeHandle::subtree` provides the same view scoped to that
subtree, including actors added dynamically through the scoped handle.

`ActorStats::message_bytes_accepted` is then `Some(total)`; ordinary actors
report `None` and do not sample message sizes. With the `metrics` feature,
each accepted sized message also updates the `actor.message.size` histogram
and `actor.message.bytes_accepted` counter. Metric handles and actor-id labels
are registered lazily on the first accepted message and cached per actor; later
accepted sends only sample `size_hint` and update those handles. Because the
byte total follows `messages_accepted`, a conflated message that is accepted
and later replaced still contributes its size even though it is never received.
Since the cached handles bind to whichever recorder is installed when the first
message is accepted, install your metrics recorder at startup, before actors
begin receiving messages. The feature
continues to enable the supervisor lifecycle counters, gauges, and histograms
as well.

## Web Console

The separate `tokio-otp-console` workspace crate can launch a web console
backed by the runtime's public snapshots, events, and actor stats:

```rust,ignore
let handle = runtime.spawn();
let console = tokio_otp_console::Console::for_runtime(&handle)
    .bind(([127, 0, 0, 1], 8080))
    .build()
    .spawn()
    .await?;

println!("console at http://{}", console.local_addr());
```

Run `cargo run -p tokio-otp-console --example console` to try it from the
workspace checkout. The console is experimental, git-only tooling and is not
a `tokio-otp` feature or dependency.
The default loopback bind remains token-free for convenient local development,
but every request is restricted to the listener address (or `localhost`) and
WebSocket browser origins must match the request host.

Non-loopback binds require an access token. Add the externally visible host
when it differs from the listener address:

```rust,ignore
let console = tokio_otp_console::Console::for_runtime(&handle)
    .bind(([0, 0, 0, 0], 8080))
    .access_token("replace-with-a-random-url-safe-token")
    .allowed_host("console.internal:8080")
    .build()
    .spawn()
    .await?;
```

API clients can send `Authorization: Bearer TOKEN`. To use the dashboard in a
browser, open `http://console.internal:8080/?token=TOKEN` once; the console
redirects to remove the token from the URL and uses an HTTP-only, same-site
cookie afterward. Treat the console as sensitive operational access: snapshots
and events include child identifiers and may include application error strings.

Host checks also apply through an SSH tunnel. Forward the same port and allow
the browser-visible authority—for example, `ssh -L 8080:host:8080 host` with
`.allowed_host("localhost:8080")`. A different local forwarding port must be
listed instead. For non-local deployments, terminate TLS at a trusted reverse
proxy so the token and console data are encrypted in transit.
