# tokio-otp

Crates for the tokio ecosystem that are inspired by Erlang/OTP.
* `tokio-supervisor` - structured task supervision
* `tokio-actor` - static actor graphs with stable ingress and actor references
* `tokio-otp` - Runtime wrappers for supervising whole graphs or individual actors

## Getting started

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
