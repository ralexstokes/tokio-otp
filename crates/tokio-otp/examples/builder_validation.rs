use std::marker::PhantomData;

use tokio_otp::{ActorContext, ActorResult, GraphBuildError, GraphBuilder, RawActor};

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

impl<M: Send + 'static> RawActor for Idle<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

fn report(label: &str, result: Result<tokio_otp::Graph, GraphBuildError>) {
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

    let mut missing = GraphBuilder::new();
    let (_ghost_slot, _ghost_ref) = missing.slot::<String>("ghost");
    report("unfilled actor slot", missing.build());

    let mut empty_name = GraphBuilder::new();
    empty_name.name("");
    empty_name.actor("worker", Idle::<()>::new());
    report("empty graph name", empty_name.build());

    // Message-type mismatches now fail at compile time:
    //
    // let mut builder = GraphBuilder::new();
    // let (slot, _worker) = builder.slot::<u32>("worker");
    // builder.define(slot, Idle::<String>::new());
    //
    // Reusing a slot token also fails at compile time because `define`
    // consumes it:
    //
    // let (slot, _worker) = builder.slot::<String>("worker");
    // builder.define(slot, Idle::<String>::new());
    // builder.define(slot, Idle::<String>::new());
}
