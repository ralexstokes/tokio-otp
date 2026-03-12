use tokio_actor::{ActorContext, ActorSpec, BuildError, GraphBuilder};

fn idle_actor(id: &str) -> ActorSpec {
    ActorSpec::from_actor(id, |_ctx: ActorContext| async { Ok(()) })
}

fn report(label: &str, result: Result<tokio_actor::Graph, BuildError>) {
    match result {
        Ok(_) => panic!("{label} unexpectedly built"),
        Err(error) => println!("{label}: {error}"),
    }
}

fn main() {
    report("empty graph", GraphBuilder::new().build());

    report(
        "zero mailbox capacity",
        GraphBuilder::new()
            .actor(idle_actor("worker"))
            .mailbox_capacity(0)
            .build(),
    );

    report(
        "duplicate actor ids",
        GraphBuilder::new()
            .actor(idle_actor("worker"))
            .actor(idle_actor("worker"))
            .build(),
    );

    report(
        "duplicate ingress names",
        GraphBuilder::new()
            .actor(idle_actor("worker"))
            .ingress("requests", "worker")
            .ingress("requests", "worker")
            .build(),
    );

    report(
        "unknown link source",
        GraphBuilder::new()
            .actor(idle_actor("worker"))
            .link("missing", "worker")
            .build(),
    );

    report(
        "unknown link target",
        GraphBuilder::new()
            .actor(idle_actor("worker"))
            .link("worker", "missing")
            .build(),
    );

    report(
        "unknown ingress target",
        GraphBuilder::new()
            .actor(idle_actor("worker"))
            .ingress("requests", "missing")
            .build(),
    );
}
