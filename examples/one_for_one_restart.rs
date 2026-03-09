use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::time::{Duration, sleep, timeout};
use tokio_supervisor::{
    BoxError, ChildSpec, Restart, Strategy, SupervisorBuilder, SupervisorEvent, SupervisorExit,
};

fn example_error(message: &'static str) -> BoxError {
    Box::new(std::io::Error::other(message))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicUsize::new(0));

    let flaky = ChildSpec::new("flaky-worker", move |ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            println!("flaky-worker started in generation {}", ctx.generation);

            if attempt == 0 {
                sleep(Duration::from_millis(100)).await;
                println!("flaky-worker failed in generation {}", ctx.generation);
                return Err(example_error("simulated startup failure"));
            }

            ctx.token.cancelled().await;
            println!("flaky-worker observed shutdown");
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let metrics = ChildSpec::new("metrics", |ctx| async move {
        println!("metrics started in generation {}", ctx.generation);
        ctx.token.cancelled().await;
        println!("metrics observed shutdown");
        Ok(())
    });

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .child(flaky)
        .child(metrics)
        .build()?;

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    loop {
        let event = timeout(Duration::from_secs(2), events.recv()).await??;
        println!("event: {event:?}");

        if let SupervisorEvent::ChildRestarted {
            id,
            old_generation,
            new_generation,
        } = event
        {
            println!("child {} restarted", id);
            if id == "flaky-worker" {
                assert_eq!(old_generation, 0);
                assert_eq!(new_generation, 1);
                break;
            }
        }
    }

    handle.shutdown();
    let exit = handle.wait().await?;
    assert_eq!(exit, SupervisorExit::Shutdown);
    println!("supervisor exited with {exit:?}");

    Ok(())
}
