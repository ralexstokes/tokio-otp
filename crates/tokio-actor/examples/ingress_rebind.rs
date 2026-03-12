use std::{error::Error, time::Duration};

use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use tokio_actor::{ActorContext, ActorSpec, Graph, GraphBuilder, GraphError, IngressError};
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("frontend", {
            let observed_tx = observed_tx.clone();
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                    }
                    Ok(())
                }
            }
        }))
        .ingress("requests", "frontend")
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");

    let (first_stop, first_task) = start_graph(&graph);
    ingress.wait_for_binding().await;
    ingress.send("first").await?;
    let first = timeout(Duration::from_secs(1), observed_rx.recv())
        .await?
        .expect("message observed");
    assert_eq!(first.as_slice(), b"first");
    println!("first run delivered: first");

    stop_graph(first_stop, first_task).await?;

    let err = ingress
        .send("between runs")
        .await
        .expect_err("stopped graph should reject sends");
    assert_eq!(
        err,
        IngressError::NotRunning {
            ingress: "requests".to_owned(),
            actor_id: "frontend".to_owned(),
        }
    );
    println!("stopped graph returned: {err}");

    let (second_stop, second_task) = start_graph(&graph);
    ingress.wait_for_binding().await;
    ingress.send("second").await?;
    let second = timeout(Duration::from_secs(1), observed_rx.recv())
        .await?
        .expect("message observed");
    assert_eq!(second.as_slice(), b"second");
    println!("second run delivered: second");

    stop_graph(second_stop, second_task).await?;
    Ok(())
}
