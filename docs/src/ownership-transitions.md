# Ownership during membership transitions

Removing an actor is not an atomic handoff of application traffic. A message
can be accepted just before the actor observes shutdown, and mailbox acceptance
only means at-most-once delivery to that incarnation. If the default
`DrainPolicy::Discard` is used, the queued message can disappear during
removal. The runtime cannot infer who should own that work, whether it is safe
to replay, or how to deduplicate its effects.

For work that must survive dynamic removal, use an application-level handoff:

```text
                       one writer for membership
sender ──message──▶ router ──Active(actor)──▶ session
                       │                         │
                       │ Evict                   │ late message
                       ▼                         │ after Evict sent
                    Evicting                     │
                    [buffer] ◀────bounce─────────┘
                       │
             remove_child completes
                       │ add same id, fresh ref
                       ▼
                 Active(replacement)
                       │ replay buffer
                       ▼
                    session
```

The protocol has four parts:

1. **Choose one membership writer.** A router owns the map from logical key to
   `Active(ActorRef)` or `Evicting(buffer)`. No other task adds, removes, or
   swaps a session, so membership state and traffic routing are serialized by
   one actor mailbox.
2. **Switch ownership before removal.** When the retiring session sends
   `Evict`, the router changes `Active` to `Evicting` before it pipelines
   `remove_child`. New router traffic goes into the buffer rather than to the
   retiring ref.
3. **Bounce the race and drain.** After requesting eviction, the retiree sends
   any late arrival back to the router and uses `DrainPolicy::Drain`. FIFO
   mailboxes preserve sequential enqueue order from one sender, so the
   retiree's `Evict` reaches the router before its later bounce. The bounce
   therefore lands in `Evicting` instead of being forwarded back in a loop.
   Configure cooperative shutdown with enough grace for this drain to finish:
   immediate abort or expiry of the grace period can skip remaining drain and
   `on_stop` work.
4. **Mint and replay.** After removal completes, the router calls `add_actor`
   with the reusable id, stores the fresh `ActorRef`, and replays the buffered
   messages in order. Old refs remain terminal and cannot address the new
   membership.

The runnable `agent_control` example implements this exact recipe. The router's
`SessionSlot::Evicting` state and pipelined removal live in
`crates/tokio-otp/examples/agent_control/router.rs`; the retiree bounce and
`DrainPolicy::Drain` live in `session.rs`; phase 7 in `main.rs` injects traffic
inside the eviction window and proves it is replayed by a replacement session.

## Put durable ownership outside the mailbox

The buffer closes the clean-removal race, but it is not durable. If the process
dies, in-memory mail can still vanish. For end-to-end delivery, append at the
transport boundary, acknowledge only after append, and redeliver unacknowledged
envelopes. Consumers should deduplicate stable envelope or effect keys. The
`agent_control` chat simulator, journal, and tool host demonstrate those three
pieces.

`remove_child` deliberately does not return undelivered `Vec<M>` values. The
supervisor handle is untyped, such a return would require downcasting, and it
would not cover crashes or already-executed effects. Delivery guarantees belong
at the journal/acknowledgement boundary. A reusable `Evicting` buffer helper may
be worthwhile once multiple applications establish the same shape, but keeping
the protocol explicit preserves the ownership decisions for now.
