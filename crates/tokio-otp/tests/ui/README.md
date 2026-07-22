# Compile-fail UI tests

`topology/` holds [trybuild](https://github.com/dtolnay/trybuild) compile-fail
cases run by `tests/topology_ui.rs`: the `#[derive(Topology)]` shape and
attribute errors, typed factory return/completeness checks, plus the two
token-API guarantees (wrong-type `define` is E0271, reusing a consumed
`ActorSlot` is E0382). Each `.rs` case has a
checked-in `.stderr` snapshot of the exact compiler output, spans included.
`topology-pass/` holds compile-pass cases for supported derive attributes,
constructor functions, and capturing factories.

## Updating snapshots on a toolchain bump

The snapshots are coupled to rustc's error rendering, so bumping the pinned
toolchain in `rust-toolchain.toml` may break them even though nothing is
wrong. Regenerate locally and review the diff:

```sh
TRYBUILD=overwrite cargo test -p tokio-otp --test topology_ui
git diff crates/tokio-otp/tests/ui
```

Commit the regenerated `.stderr` files together with the toolchain bump.
This must happen locally: `just ci-nix` runs in a read-only sandbox and can
only report the mismatch, not overwrite the snapshots.
