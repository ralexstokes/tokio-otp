# Tokio Supervisor Remaining Work

This document carries forward the parts of the old handoff `SPEC.md` that are
not yet reflected by the codebase at commit
`2fe64feff2d853c570569f899474844740e7077b`.

## Observability

The crate already exposes `SupervisorEvent` and has light internal tracing, but broader observability is still open:

* richer tracing spans and logs across the supervision lifecycle
* metrics for restarts, shutdowns, failures, and running children
* clearer observability guidance and examples beyond the basic event subscriber example

## Hardening and coverage

The core runtime is implemented and tested, but there is still room for targeted hardening:

* direct coverage for `ShutdownMode::Cooperative` returning `SupervisorError::ShutdownTimedOut`
* direct coverage for exponential backoff behavior
* broader event-surface verification
* general race and failure-path hardening

## Feature backlog

The remaining feature backlog is:

* dynamic add/remove child at runtime

* readiness protocol
* jittered exponential backoff

* health probes

## Out of scope

* required vs optional children
* distributed supervision
* actor runtime or mailbox abstractions

## Notes

Some sections of the old `SPEC.md` were implementation planning material rather than current-state specification:

* implementation order
* dependency recommendations
* suggested file sketches
* broad testing matrix beyond what now exists

Those sections were intentionally not copied into `SPEC_2fe64fe.md`.
