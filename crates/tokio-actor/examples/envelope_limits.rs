use std::{error::Error, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder, IngressError, SendError};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (peer_error_tx, mut peer_error_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("source", {
            let peer_error_tx = peer_error_tx.clone();
            move |mut ctx: ActorContext| {
                let peer_error_tx = peer_error_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        if envelope.as_slice() == b"go" {
                            let error = ctx
                                .send("sink", vec![0_u8; 5])
                                .await
                                .expect_err("oversized peer envelope should be rejected");
                            peer_error_tx.send(error).expect("receiver alive");
                        }
                    }
                    Ok(())
                }
            }
        }))
        .actor(ActorSpec::from_actor(
            "sink",
            |mut ctx: ActorContext| async move {
                while ctx.recv().await.is_some() {}
                Ok(())
            },
        ))
        .link("source", "sink")
        .ingress("requests", "source")
        .max_envelope_bytes(4)
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ingress.wait_for_binding().await;

    match ingress.send(vec![0_u8; 5]).await {
        Err(IngressError::EnvelopeTooLarge {
            ingress,
            actor_id,
            envelope_len,
            max_envelope_bytes,
        }) => {
            println!(
                "ingress `{ingress}` rejected {envelope_len} bytes for `{actor_id}` (limit {max_envelope_bytes} bytes)"
            );
        }
        Ok(()) => panic!("oversized ingress envelope unexpectedly succeeded"),
        Err(other) => panic!("unexpected ingress error: {other}"),
    }

    ingress.send(Envelope::from_static(b"go")).await?;

    match timeout(Duration::from_secs(1), peer_error_rx.recv())
        .await?
        .expect("peer send result received")
    {
        SendError::EnvelopeTooLarge {
            actor_id,
            envelope_len,
            max_envelope_bytes,
        } => {
            println!(
                "peer send to `{actor_id}` rejected {envelope_len} bytes (limit {max_envelope_bytes} bytes)"
            );
        }
        other => panic!("unexpected peer send error: {other}"),
    }

    stop.cancel();
    task.await??;
    Ok(())
}
