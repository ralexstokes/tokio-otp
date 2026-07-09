use std::{error::Error, marker::PhantomData};

use tokio::sync::mpsc;
use tokio_actor::{ActorContext, ActorResult, GraphBuilder, MessageHandler, SendError};

struct Observe<M> {
    observed: mpsc::UnboundedSender<M>,
    _message: PhantomData<fn(M)>,
}

impl<M> Observe<M> {
    fn new(observed: mpsc::UnboundedSender<M>) -> Self {
        Self {
            observed,
            _message: PhantomData,
        }
    }
}

impl<M> Clone for Observe<M> {
    fn clone(&self) -> Self {
        Self {
            observed: self.observed.clone(),
            _message: PhantomData,
        }
    }
}

impl<M: Send + 'static> MessageHandler for Observe<M> {
    type Msg = M;

    async fn handle(&mut self, message: M, _ctx: &ActorContext<M>) -> ActorResult {
        self.observed.send(message).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let frontend = builder.add(Observe::<String>::new(observed_tx));
    let graph = builder.build()?;

    let handle = graph.spawn()?;
    frontend.send("first".to_owned()).await?;
    println!("observed {:?}", observed_rx.recv().await);
    handle.shutdown_and_wait().await?;

    match frontend.try_send("between-runs".to_owned()) {
        Err(SendError::ActorTerminated { actor_id }) => {
            println!("`{actor_id}` has no running graph instance");
        }
        other => println!("unexpected send result: {other:?}"),
    }

    let handle = graph.spawn()?;
    frontend.send("second".to_owned()).await?;
    println!("observed {:?}", observed_rx.recv().await);
    handle.shutdown_and_wait().await?;

    Ok(())
}
