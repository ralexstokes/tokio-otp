# tokio-supervisor

Structured task supervision for tokio.
Inspired by Erlang/OTP-style supervisor trees.

**Note:** this library only provides supervisor trees with no distributed supervision, or anything like an actor runtime.

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
