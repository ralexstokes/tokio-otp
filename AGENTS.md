# tokio-otp

Erlang/OTP-like functionality for the `tokio` ecosystem, organized as a Cargo workspace.

## Structure

* `crates/tokio-supervisor` — structured task supervision (supervision trees, restart strategies)
* `crates/tokio-actor` — static typed actor graphs with restart-stable actor refs
* `crates/tokio-otp` — umbrella crate tying supervision and actors into an OTP-style runtime
* `crates/tokio-otp-console` — [experimental] web-based dashboard for supervisor trees (axum + WebSocket)
* `docs/` — mdBook tutorial (`just build-book` / `just serve-book`)
* `nix/crane-checks.nix` + `flake.nix` — authoritative CI check definitions

## Development workflow

* During development, use `just ci` — a fast local mirror of CI that reuses the cargo cache. It runs fmt, clippy, build, tests (including doctests), nixfmt, and the book build.

* Before pushing to a git remote, `just ci-nix` must pass cleanly. It runs `nix flake check`, exactly what GitHub Actions runs. It builds with `--locked` from a clean source tree, so `just ci` passing does not guarantee `just ci-nix` passes.

* The `just ci` recipes mirror the check definitions in `nix/crane-checks.nix` — if you change flags in one, update the other.

* If you add new files that would be covered by `nix flake check`, stage them first (`git add`). Nix builds from the git tree and cannot see untracked files.

## Conventions

* Formatting and linting use the nightly toolchain: `cargo +nightly fmt` and `cargo +nightly clippy`. Clippy runs with `-D warnings` — new code must be warning-free.

* Builds and tests run with `--workspace --all-targets --all-features`; keep feature-gated code (`metrics`, `serde`, `console`) compiling under those flags.

* Nix files are formatted with `nixfmt` (checked by `just nixfmt-check`).
