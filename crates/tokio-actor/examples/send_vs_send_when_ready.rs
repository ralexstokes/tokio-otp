use std::{error::Error, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{ActorContext, ActorResult, GraphBuilder, MessageHandler, SendError};

#[derive(Clone)]
struct Sink {
    observed: mpsc::UnboundedSender<&'static str>,
}

impl MessageHandler for Sink {
    type Msg = &'static str;

    async fn handle(
        &mut self,
        message: &'static str,
        _ctx: &ActorContext<&'static str>,
    ) -> ActorResult {
        self.observed.send(message).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let sink_ref = builder.actor(
        "sink",
        Sink {
            observed: observed_tx,
        },
    );
    let actor_set = builder.build()?.into_actor_set()?;
    let sink = actor_set.actor("sink").expect("sink exists").clone();

    match sink_ref.send("while-stopped").await {
        Err(SendError::ActorNotRunning { actor_id }) => {
            println!("send failed because `{actor_id}` is stopped");
        }
        other => println!("unexpected send result: {other:?}"),
    }

    let mut ready_ref = sink_ref.clone();
    let send_task = tokio::spawn(async move { ready_ref.send_when_ready("after-start").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!("send_when_ready is waiting for the actor to start");

    let stop = tokio_util::sync::CancellationToken::new();
    let run = tokio::spawn({
        let stop = stop.clone();
        async move { sink.run_until(stop.cancelled()).await }
    });

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await?
        .expect("message observed");
    println!("sink observed `{observed}`");
    send_task.await??;

    stop.cancel();
    run.await??;
    Ok(())
}
