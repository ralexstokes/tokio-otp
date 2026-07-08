use std::{error::Error, marker::PhantomData};

use tokio::sync::mpsc;
use tokio_actor::{
    ActorContext, ActorResult, Graph, GraphBuilder, GraphError, MessageHandler, SendError,
};
use tokio_util::sync::CancellationToken;

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

fn spawn_graph(
    graph: Graph,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<Result<(), GraphError>>,
) {
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });
    (stop, task)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let mut frontend = builder.actor("frontend", Observe::<String>::new(observed_tx));
    let graph = builder.build()?;

    let (stop, task) = spawn_graph(graph.clone());
    frontend.wait_for_binding().await;
    frontend.send("first".to_owned()).await?;
    println!("observed {:?}", observed_rx.recv().await);
    stop.cancel();
    task.await??;

    match frontend.try_send("between-runs".to_owned()) {
        Err(SendError::ActorNotRunning { actor_id }) => {
            println!("`{actor_id}` is unbound between graph runs");
        }
        other => println!("unexpected send result: {other:?}"),
    }

    let (stop, task) = spawn_graph(graph);
    frontend.send_when_ready("second".to_owned()).await?;
    println!("observed {:?}", observed_rx.recv().await);
    stop.cancel();
    task.await??;

    Ok(())
}
