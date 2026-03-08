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

ci:
    nix flake check --no-update-lock-file

doc:
    cargo doc --workspace --no-deps --open

build-book:
    mdbook build docs

serve-book:
    mdbook serve docs
