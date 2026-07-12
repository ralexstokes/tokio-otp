# The recipes used by `ci` mirror the checks defined in nix/crane-checks.nix
# and flake.nix (the authoritative CI definitions) — keep them in sync.

default:
    @just --list

fmt:
    cargo +nightly fmt --all --check

lint:
    cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings

check:
    cargo check --workspace --all-targets --all-features

build:
    cargo build --workspace --all-targets --all-features

test:
    cargo test --workspace --all-targets --all-features
    cargo test --workspace --doc --all-features

doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

nixfmt-check:
    nixfmt --check flake.nix nix/crane-checks.nix

# Fast local CI mirror — reuses the local cargo cache and incremental builds.
ci: fmt lint build test doc-check nixfmt-check build-book

# Exactly what GitHub Actions runs; use before pushing or when touching nix files.
ci-nix:
    nix flake check --no-update-lock-file

doc:
    cargo doc --workspace --no-deps --open

build-book:
    mdbook build docs

serve-book:
    mdbook serve docs
