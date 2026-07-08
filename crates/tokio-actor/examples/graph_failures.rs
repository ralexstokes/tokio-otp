use std::{error::Error, future::pending, io};

use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuilder, GraphError};

#[derive(Clone)]
struct Fails;

impl Actor for Fails {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        Err(io::Error::other("boom").into())
    }
}

#[derive(Clone)]
struct RunsForever;

impl Actor for RunsForever {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        pending::<()>().await;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", RunsForever);
    builder.actor("failing", Fails);
    let graph = builder.build()?;

    match graph.run_until(pending::<()>()).await {
        Err(GraphError::ActorFailed { actor_id, source }) => {
            println!("graph stopped because `{actor_id}` failed: {source}");
        }
        other => println!("unexpected graph result: {other:?}"),
    }

    Ok(())
}
