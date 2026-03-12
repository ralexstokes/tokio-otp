#[cfg(feature = "metrics")]
use metrics_exporter_prometheus::PrometheusBuilder;
#[cfg(feature = "metrics")]
use std::{error::Error, thread, time::Duration};
#[cfg(feature = "metrics")]
use tokio::{sync::mpsc, time::timeout};
#[cfg(feature = "metrics")]
use tokio_actor::{ActorContext, ActorSpec, BlockingOptions, Envelope, GraphBuilder};
#[cfg(feature = "metrics")]
use tokio_util::sync::CancellationToken;

#[cfg(feature = "metrics")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let recorder = PrometheusBuilder::new().install_recorder()?;
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .name("metrics-demo")
        .actor(ActorSpec::from_actor(
            "dispatcher",
            |mut ctx: ActorContext| async move {
                let sink = ctx.peer("sink").expect("linked sink exists");

                while let Some(envelope) = ctx.recv().await {
                    let sink = sink.clone();
                    let _task =
                        ctx.spawn_blocking(BlockingOptions::named("uppercase"), move |_job| {
                            thread::sleep(Duration::from_millis(20));
                            let uppercased: Vec<u8> = envelope
                                .as_slice()
                                .iter()
                                .map(|byte| byte.to_ascii_uppercase())
                                .collect();
                            sink.blocking_send(uppercased)?;
                            Ok(())
                        })?;
                }

                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor("sink", {
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
        .link("dispatcher", "sink")
        .ingress("requests", "dispatcher")
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello metrics"))
        .await?;
    let envelope = timeout(Duration::from_secs(1), observed_rx.recv())
        .await?
        .expect("processed message received");
    println!(
        "processed message: {}",
        std::str::from_utf8(envelope.as_slice())?
    );

    stop.cancel();
    task.await??;

    println!("# Prometheus snapshot");
    println!("{}", recorder.render());

    Ok(())
}

#[cfg(not(feature = "metrics"))]
fn main() {
    eprintln!("run this example with: cargo run --example metrics --features metrics");
}
