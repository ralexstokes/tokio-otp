use std::marker::PhantomData;

use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuildError, GraphBuilder};

struct Idle<M>(PhantomData<fn(M)>);

impl<M> Idle<M> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M> Clone for Idle<M> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<M: Send + 'static> Actor for Idle<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

fn report(label: &str, result: Result<tokio_actor::Graph, GraphBuildError>) {
    match result {
        Ok(_) => panic!("{label} unexpectedly built"),
        Err(error) => println!("{label}: {error}"),
    }
}

fn main() {
    report("empty graph", GraphBuilder::new().build());

    let mut zero_capacity = GraphBuilder::new();
    zero_capacity.mailbox_capacity(0);
    zero_capacity.actor("worker", Idle::<()>::new());
    report("zero mailbox capacity", zero_capacity.build());

    let mut duplicate = GraphBuilder::new();
    duplicate.actor("worker", Idle::<()>::new());
    duplicate.actor("worker", Idle::<()>::new());
    report("duplicate actor ids", duplicate.build());

    let mut mismatch = GraphBuilder::new();
    mismatch.declare::<u32>("worker");
    mismatch.actor("worker", Idle::<String>::new());
    report("message type mismatch", mismatch.build());

    let mut missing = GraphBuilder::new();
    missing.declare::<String>("ghost");
    report("declared but missing actor", missing.build());

    let mut empty_name = GraphBuilder::new();
    empty_name.name("");
    empty_name.actor("worker", Idle::<()>::new());
    report("empty graph name", empty_name.build());
}
