use std::marker::PhantomData;

use tokio_actor::{ActorContext, ActorResult, MessageHandler, Topology};

#[derive(Clone)]
struct Worker;

impl MessageHandler for Worker {
    type Msg = ();

    async fn handle(&mut self, _message: (), _ctx: &ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

#[derive(Topology)]
struct GenericTopology<T> {
    worker: Worker,
    _marker: PhantomData<T>,
}

fn main() {}
