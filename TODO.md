* [] v0.1 SPEC gaps
  - fix the shutdown-vs-zero-delay-restart race so shutdown consistently returns `SupervisorExit::Shutdown`
* [] tracing, metrics, observability
* [] hardening
* [] required vs optional children
* [] per-child restart intensity
* [] nested supervisors as first-class child specs
  - or consider just a supervisor as child adapter
* [] dynamic add/remove child at runtime
* [] readiness protocol
* [] jittered exponential backoff
* [] health probes
* [] distributed supervision
* [] actor runtime / mailboxes
