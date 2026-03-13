## Structure

Set of crates for Erlang/OTP-like functionality in the `tokio` ecosystem.

Currently:
* `tokio-supervisor` - structured task supervision
* `tokio-actor` - graphs of communicating tasks

## Development

* When linting new code, this repo uses the nightly variants, i.e. `cargo +nightly fmt` and `cargo +nightly clippy`.

* If you add new files that would be covered by `nix flake check`, make sure they are staged first. Otherwise `nix` cannot easily see them.

* Before final handoff of any changes to Rust code, `just ci` should pass cleanly.
