# Spec: Simplify `ActorRef` send API around a tri-state binding

Resolves TODO.md item 1 in full, plus the pieces of items 2 and 6 that share
its machinery. Approved design decision: **whether a send waits or errors is
decided by supervision policy, not by the caller** — a ref whose actor is
restarting waits; a ref whose actor is gone for good errors.

## 1. Target public surface

`ActorRef<M>` keeps exactly these messaging methods, all taking `&self`:

```rust
pub async fn send(&self, message: M) -> Result<(), SendError>;
pub fn try_send(&self, message: M) -> Result<(), SendError>;
pub async fn call<T>(&self, message: impl FnOnce(Reply<T>) -> M) -> Result<T, CallError>;
```

plus the existing `id()`. **Deleted:** `send_when_ready`, `blocking_send`,
`wait_for_binding` (and the private `wait_for_next_binding` /
`wait_for_rebind` become implementation details of `send`).

Semantics:

- `send` — waits until the actor has a bound mailbox (riding through restart
  windows, including retrying a message returned by a mailbox that closed
  mid-send, as `send_when_ready` does today), then waits for capacity. It
  returns an error only when the binding reaches a **terminated** state (or
  the binding source is dropped). This absorbs `send_when_ready`.
- `try_send` — synchronous, fail-fast on everything: no binding, terminated,
  full, or closed. It is callable from blocking code; this absorbs
  `blocking_send`, which today is an exact duplicate of `try_send`.
- `call` — unchanged shape, but built on the new waiting `send`, so it no
  longer fails with `ActorNotRunning` during a restart window (TODO item 1,
  second bullet).
- No built-in deadlines or cancellation tokens. Callers compose with
  `tokio::time::timeout` / `select!`. Document on `send` and `call` that
  cancelling the future mid-wait drops the message (they are not
  cancel-safe in the "message returned" sense).

The `&mut self` problem disappears because waiting methods clone the
`watch::Receiver` internally (`watch::Receiver::wait_for` needs `&mut`; a
fresh clone observes the current value, so this is correct).

## 2. Tri-state binding

Replace the `Option<MailboxRef<M>>` slot in `BindingCore`
(`crates/tokio-actor/src/binding.rs`) with a tri-state:

```rust
enum BindingState<M> {
    /// Not yet started, or between restarts — a new mailbox is expected.
    Unbound,
    Bound(MailboxRef<M>),
    /// No restart is scheduled; sends fail fast. A later rebind (e.g. a
    /// graph rerun) may still transition back to Bound.
    Terminated,
}
```

- Initial state: `Unbound`.
- `send` waits through `Unbound`; errors on `Terminated`; errors if the
  watch sender is dropped.
- `try_send` errors immediately on `Unbound` (`ActorNotRunning`) and on
  `Terminated` (see error taxonomy below).
- `Terminated` is not poisoned: a subsequent start (graph rerun, actor
  re-added) rebinds and later sends succeed. The contract is only that
  waiters never hang on an actor with no scheduled restart.

### Who sets which state

The clearing side must know whether a rebind is expected. Define in
`tokio-actor` (which must not depend on `tokio-supervisor`) a disposition
the runner is configured with, e.g.:

```rust
pub enum RebindPolicy {
    Always,      // Restart::Permanent
    OnFailure,   // Restart::Transient
    Never,       // Restart::Temporary, and the default
}
```

Rules for the state written when a run ends:

| Run ended because…              | `Always`    | `OnFailure` | `Never`    |
|---------------------------------|-------------|-------------|------------|
| shutdown was requested          | Terminated  | Terminated  | Terminated |
| actor failed (error or panic)   | Unbound     | Unbound     | Terminated |
| actor exited cleanly            | Unbound     | Terminated  | Terminated |
| future dropped without a result | Unbound     | Unbound     | Terminated |

("Unbound" here means "expect a rebind"; the supervisor will start a new
run which rebinds.) `RunnableActor::run_until`
(`crates/tokio-actor/src/actor_set.rs`) knows why a run ended (its shutdown
future fired vs. the actor result), so it applies the table explicitly; the
`BindingGuard` drop path is the fallback row. Bare `Graph::run_until` runs
actors once with no per-actor restart, so graph-run mode clears to
`Terminated` when each actor's run ends.

Threading: `RunnableActor` gains a way to configure its `RebindPolicy`
(analogous to the existing `set_registry`), defaulting to `Never`. The
`tokio-otp` builder (`crates/tokio-otp/src/builder.rs`) maps each actor's
`Restart::{Permanent, Transient, Temporary}` from `ActorConfig` onto it when
building child specs. `RuntimeHandle::add_actor` does the same for dynamic
actors; `RuntimeHandle::remove_actor` explicitly transitions the binding to
`Terminated`.

The run-end table alone cannot cover the case where the last run ended
expecting a rebind but the supervisor then gives up (restart intensity
exhausted, child removed, supervisor exit): the binding would stay `Unbound`
and senders would hang. The child-spec factory closure built in
`actor_child_spec` therefore holds a drop guard that terminates the binding
(and deregisters the actor) when the supervisor drops the spec — the moment
after which no restart can ever be started. To keep a late run teardown from
racing this, `unbind` only downgrades a currently-`Bound` state and never
regresses `Terminated`.

### Registry eviction (TODO item 6, first bullet)

"Binding transitioned to `Terminated`" is exactly the event whose absence
makes `ActorRegistry` entries go stale. When a registered actor's binding
terminates, evict its registry entry, so `actor_ids()` / `contains()` /
`actor_ref` stop advertising actors that can never be sent to. The
`RunnableActor` layer holds both the binding core and the registry handle
(via `set_registry` / `register_with`), so the transition points above can
perform the eviction. Cover: transient/temporary actors exiting for good,
`remove_actor`, and direct `remove_child` of a dynamic actor.

## 3. Error taxonomy

Add a distinct variant to `SendError`
(`crates/tokio-actor/src/error.rs`):

```rust
/// The target actor has terminated and no restart is scheduled.
#[error("actor `{actor_id}` has terminated")]
ActorTerminated { actor_id: String },
```

- `send`: the only non-internal errors are `ActorTerminated` (terminated
  state, or watch sender dropped). `MailboxFull` never surfaces (it waits);
  `MailboxClosed` is handled internally by the rebind loop and surfaces as
  `ActorTerminated` if the loop ends terminally.
- `try_send`: `ActorNotRunning` (Unbound), `ActorTerminated` (Terminated),
  `MailboxFull`, `MailboxClosed` as today.
- `CallError` is unchanged (`Send(#[from] SendError)` picks up the new
  variant).

## 4. Readiness helper (TODO item 2, partial)

`wait_for_binding` is deleted, and its pre-send use is obsolete (`send`
waits). Its readiness-probe use moves to the runtime:

```rust
impl RuntimeHandle {
    /// Resolves when every currently-registered child reports running.
    pub async fn wait_until_running(&self) -> Result<(), SupervisorError>;
}
```

Implement via the existing `subscribe_snapshots()`
(`watch::Receiver<SupervisorSnapshot>`) — wait for a snapshot where all
children are running; no event-subscription race. (The full item 2,
`wait_for_child_restart`, is out of scope here.)

If a `tokio-actor`-only test or example needs a readiness probe without a
runtime, prefer restructuring around the waiting `send`; do not keep
`wait_for_binding` public for it.

## 5. Observability

- Delete `MessageOperation::BlockingSend` and its `"blocking_send"` label
  (`crates/tokio-actor/src/observability.rs`).
- `send` keeps emitting `MessageOperation::Send` per attempt as
  `send_when_ready` does today; `try_send` keeps `TrySend`.

## 6. Call-site and docs migration

- README first code sample and the book (`docs/src/`): remove
  `let mut … = self.….clone();` patterns and per-ref
  `wait_for_binding().await` loops; `send_when_ready` mentions become
  `send`.
- `crates/tokio-actor/examples/send_vs_send_when_ready.rs`: rework (and
  rename) to contrast `send` vs `try_send` under restarts.
- `crates/tokio-actor/examples/blocking_work.rs` and the `actor_set.rs`
  test: `blocking_send` → `try_send`.
- All other examples/tests that call `wait_for_binding` or
  `send_when_ready` (see `grep -rn` across `crates/`): most pre-send waits
  can simply be deleted; genuine readiness assertions use
  `wait_until_running`.
- Update TODO.md: item 1 done; strike the covered bullets from items 2
  and 6.

## 7. New behavior tests

At minimum:

1. `send` blocks during a restart window and delivers after the rebind
   (Permanent actor, supervised).
2. `send` returns `ActorTerminated` promptly when a Temporary (or cleanly
   exiting Transient) actor stops — no hang.
3. `send` to a never-started graph waits, then delivers once the graph
   runs; errors if the graph is dropped instead.
4. `try_send` fail-fast on Unbound and Terminated with the right variants.
5. `call` succeeds across a restart window (regression for the old
   `ActorNotRunning` failure).
6. Registry no longer lists an actor after it terminates for good /
   `remove_actor`.
7. `wait_until_running` resolves after `spawn()` with multiple children.

## 8. Acceptance criteria

- `just ci` passes (fmt, clippy `-D warnings`, build/tests with
  `--workspace --all-targets --all-features`, doctests, nixfmt, book).
- Examples are only *built* in CI — run every touched example manually
  (`cargo run --example …` in the owning crate) and confirm it completes.
- `git add` any new files (nix builds from the git tree).
- No public method on `ActorRef` takes `&mut self`.
