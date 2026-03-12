use std::{error::Error, future::pending, io, time::Duration};

use tokio::{task::JoinHandle, time::timeout};
use tokio_actor::{ActorContext, ActorSpec, Graph, GraphBuilder, GraphError};
use tokio_util::sync::CancellationToken;

fn start_graph(graph: &Graph) -> (CancellationToken, JoinHandle<Result<(), GraphError>>) {
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });
    (stop, task)
}

async fn stop_graph(
    stop: CancellationToken,
    task: JoinHandle<Result<(), GraphError>>,
) -> Result<(), Box<dyn Error>> {
    stop.cancel();

    let joined = timeout(Duration::from_secs(1), task).await?;
    let result = joined?;
    result?;
    Ok(())
}

async fn demonstrate_already_running() -> Result<(), Box<dyn Error>> {
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |mut ctx: ActorContext| async move {
                while ctx.recv().await.is_some() {}
                Ok(())
            },
        ))
        .ingress("requests", "worker")
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let (stop, task) = start_graph(&graph);

    ingress.wait_for_binding().await;
    let err = graph
        .run_until(async {})
        .await
        .expect_err("same graph cannot run twice");
    assert!(matches!(err, GraphError::AlreadyRunning));
    println!("concurrent rerun returned: {err}");

    stop_graph(stop, task).await?;
    Ok(())
}

async fn demonstrate_actor_stopped() -> Result<(), Box<dyn Error>> {
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async { Ok(()) },
        ))
        .build()?;

    let err = graph
        .run_until(pending::<()>())
        .await
        .expect_err("clean actor exit should fail the graph");
    assert!(matches!(
        &err,
        GraphError::ActorStopped { actor_id } if actor_id == "worker"
    ));
    println!("clean actor exit returned: {err}");

    Ok(())
}

async fn demonstrate_actor_failed() -> Result<(), Box<dyn Error>> {
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async { Err(io::Error::other("boom").into()) },
        ))
        .build()?;

    let err = graph
        .run_until(pending::<()>())
        .await
        .expect_err("actor error should fail the graph");

    match &err {
        GraphError::ActorFailed { actor_id, source } => {
            assert_eq!(actor_id, "worker");
            println!("actor failure returned: {source}");
        }
        other => panic!("unexpected result: {other:?}"),
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    demonstrate_already_running().await?;
    demonstrate_actor_stopped().await?;
    demonstrate_actor_failed().await?;
    Ok(())
}
