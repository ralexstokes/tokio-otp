use tokio_actor::{ActorContext, ActorResult, Actor, Topology};

#[derive(Clone)]
struct Worker;

impl Actor for Worker {
    type Msg = ();

    async fn handle(&mut self, _message: (), _ctx: &ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

#[derive(Topology)]
struct TupleTopology(Worker);

fn main() {}
