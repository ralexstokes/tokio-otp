use tokio_actor::{ActorContext, ActorResult, GraphBuilder, MessageHandler};

#[derive(Clone)]
struct Worker;

impl MessageHandler for Worker {
    type Msg = ();

    async fn handle(&mut self, _message: (), _ctx: &ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

fn main() {
    let mut builder = GraphBuilder::new();
    let (slot, _worker) = builder.slot::<()>("worker");
    builder.define(slot, Worker);
    builder.define(slot, Worker);
}
