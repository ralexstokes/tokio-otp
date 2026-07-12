# Bounded Request/Reply

`ActorRef::call` builds request/reply on the ordinary actor mailbox: it creates
a one-shot `Reply<T>`, puts that reply handle in your message, sends the
message, and waits for the actor to answer. Deadlines remain caller-owned, so
bound the whole operation with `tokio::time::timeout`:

```rust,no_run
use std::time::Duration;
use tokio::time::timeout;
use tokio_otp::{ActorRef, Reply};

enum AccountMsg {
    Balance(Reply<u64>),
}

async fn balance(
    account: &ActorRef<AccountMsg>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let balance = timeout(
        Duration::from_millis(250),
        account.call(AccountMsg::Balance),
    )
    .await??;
    Ok(balance)
}
```

The timeout covers both phases of a call:

1. **Delivery:** `call` waits for the same conditions as `send`. Before the
   actor starts, or during an expected restart, it waits for a mailbox to bind.
   With a full FIFO mailbox, it waits for capacity. If the deadline expires in
   this phase, cancelling the future drops the request before acceptance.
2. **Reply:** after the mailbox accepts the request, `call` waits on its
   one-shot reply channel. If the deadline expires now, only the caller's wait
   is cancelled. The accepted request remains in the mailbox, the actor may
   still process it, and `Reply::send` silently discards a late result.

This boundary means that a timed-out call can have an **unknown outcome**. The
caller cannot generally tell whether the request never reached the actor, is
still queued, is currently running, or completed after the deadline.

## Side effects and retries

For read-only operations, ignoring a late reply is often enough. For commands
that write to a database, charge a card, publish an event, or otherwise affect
an external system, do not treat a timeout as proof that nothing happened.
Give each logical operation an idempotency key that the actor and external
system persist, or provide a reconciliation query that lets the caller learn
the final status. Retry with the same key rather than creating a second
logical operation.

Cancellation of the `call` future is not a cancellation signal for actor work.
If a protocol needs cooperative cancellation, model it explicitly in the
message type and define what happens when cancellation races with completion.

## Backpressure and restarts

The composed timeout deliberately includes mailbox backpressure and restart
backoff. Choose a deadline that covers the queueing delay your service is
willing to tolerate:

- Use `try_send` when fail-fast delivery is more useful than a reply.
- Use `timeout(..., actor.call(...))` when the caller can wait for capacity or
  a short restart window, but needs a firm end-to-end deadline.
- Do not use `call` with a conflating mailbox. A newer value can replace the
  request, causing `CallError::ReplyDropped`.

A call waiting during an expected restart can succeed after the new actor
incarnation binds. A request accepted by the old incarnation before it stops
is still subject to the at-most-once delivery contract: it may be lost with
that incarnation, in which case the reply channel closes with
`CallError::ReplyDropped`.
