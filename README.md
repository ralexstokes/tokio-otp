# tokio-otp

Crates for the tokio ecosystem that are inspired by Erlang/OTP.
* `tokio-supervisor` - structured task supervision
* `tokio-actor` - static typed actor graphs with restart-stable actor references
* `tokio-otp` - Runtime wrappers for supervising whole graphs or individual actors

## Getting started

Start with the tutorial book under [`docs/`](docs/src/SUMMARY.md), e.g. `just serve-book`.

Check the Rust docs for more information, e.g. `just doc`.

## Development

Use the flake for both local tooling and CI:

```sh
nix develop
just ci
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
