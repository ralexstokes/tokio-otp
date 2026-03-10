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
    let nested_attempts = Arc::new(AtomicUsize::new(0));

    let nested_worker = {
        let nested_attempts = Arc::clone(&nested_attempts);
        ChildSpec::new("nested-worker", move |ctx| {
            let nested_attempts = Arc::clone(&nested_attempts);
            async move {
                println!("nested-worker started in generation {}", ctx.generation);

                if nested_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    sleep(Duration::from_millis(100)).await;
                    println!("nested-worker failed");
                    return Err(example_error("simulated nested failure"));
                }

                ctx.token.cancelled().await;
                println!("nested-worker observed shutdown");
                Ok(())
            }
        })
        .restart(Restart::Transient)
    };

    let nested_supervisor = SupervisorBuilder::new().child(nested_worker).build()?;

    let metrics = ChildSpec::new("metrics", |ctx| async move {
        println!("metrics started in generation {}", ctx.generation);
        ctx.token.cancelled().await;
        println!("metrics observed shutdown");
        Ok(())
    })
    .restart(Restart::Permanent);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .child(nested_supervisor.into_child_spec("nested-pipeline"))
        .child(metrics)
        .build()?;

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    let mut saw_nested_restart = false;
    let mut metrics_generation_zero = false;

    loop {
        let event = timeout(Duration::from_secs(2), events.recv()).await??;
        println!("event: {event:?}");

        match event {
            SupervisorEvent::ChildStarted { id, generation }
                if id == "metrics" && generation == 0 =>
            {
                metrics_generation_zero = true;
            }
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested-pipeline"
                && generation == 0
                && matches!(
                    *event,
                    SupervisorEvent::ChildRestarted {
                        ref id,
                        old_generation: 0,
                        new_generation: 1,
                    } if id == "nested-worker"
                ) =>
            {
                saw_nested_restart = true;
            }
            _ => {}
        }

        if saw_nested_restart && metrics_generation_zero {
            break;
        }
    }

    println!("nested subtree recovered internally without restarting outer siblings");

    handle.shutdown();
    let exit = handle.wait().await?;
    assert_eq!(exit, SupervisorExit::Shutdown);
    println!("supervisor exited with {exit:?}");

    Ok(())
}
