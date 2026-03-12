use std::{error::Error, time::Duration};

use tokio::{
    sync::{mpsc, oneshot, watch},
    time::timeout,
};
use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder, IngressError, SendError};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (source_release_tx, source_release_rx) = watch::channel(false);
    let (sink_release_tx, sink_release_rx) = watch::channel(false);
    let (source_ready_tx, source_ready_rx) = oneshot::channel();
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("source", {
            let source_ready_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(source_ready_tx)));
            let source_release_rx = source_release_rx.clone();
            move |mut ctx: ActorContext| {
                let source_ready_tx = std::sync::Arc::clone(&source_ready_tx);
                let mut source_release_rx = source_release_rx.clone();
                async move {
                    source_release_rx
                        .wait_for(|released| *released)
                        .await
                        .expect("source release sender alive");

                    while let Some(envelope) = ctx.recv().await {
                        match envelope.as_slice() {
                            b"first" => {
                                if let Some(tx) =
                                    source_ready_tx.lock().expect("mutex not poisoned").take()
                                {
                                    let _ = tx.send(());
                                }
                            }
                            b"fill-peer" => {
                                ctx.try_send("sink", Envelope::from_static(b"queued"))?;
                                match ctx.try_send("sink", Envelope::from_static(b"overflow")) {
                                    Err(SendError::MailboxFull { actor_id }) => {
                                        println!("peer mailbox is full: {actor_id}");
                                    }
                                    Ok(()) => panic!("second peer send unexpectedly succeeded"),
                                    Err(other) => panic!("unexpected peer send error: {other}"),
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                }
            }
        }))
        .actor(ActorSpec::from_actor("sink", {
            let observed_tx = observed_tx.clone();
            let sink_release_rx = sink_release_rx.clone();
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                let mut sink_release_rx = sink_release_rx.clone();
                async move {
                    sink_release_rx
                        .wait_for(|released| *released)
                        .await
                        .expect("sink release sender alive");

                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("observer alive");
                    }
                    Ok(())
                }
            }
        }))
        .link("source", "sink")
        .ingress("requests", "source")
        .mailbox_capacity(1)
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ingress.wait_for_binding().await;
    ingress.try_send(Envelope::from_static(b"first"))?;
    match ingress.try_send(Envelope::from_static(b"second")) {
        Err(IngressError::MailboxFull { ingress, actor_id }) => {
            println!("ingress `{ingress}` is backpressured by `{actor_id}`");
        }
        Ok(()) => panic!("second ingress send unexpectedly succeeded"),
        Err(other) => panic!("unexpected ingress send error: {other}"),
    }

    source_release_tx.send(true)?;
    source_ready_rx
        .await
        .expect("source consumed the first message");

    ingress.send(Envelope::from_static(b"fill-peer")).await?;

    sink_release_tx.send(true)?;
    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await?
        .expect("sink received the queued envelope");
    println!(
        "sink eventually received: {}",
        std::str::from_utf8(observed.as_slice())?
    );

    stop.cancel();
    task.await??;
    Ok(())
}
