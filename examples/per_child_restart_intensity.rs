use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::time::{Duration, sleep, timeout};
use tokio_supervisor::{
    BackoffPolicy, BoxError, ChildSpec, Restart, RestartIntensity, SupervisorBuilder,
    SupervisorEvent, SupervisorExit,
};

fn example_error(message: &'static str) -> BoxError {
    Box::new(std::io::Error::other(message))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let warm_cache_attempts = Arc::new(AtomicUsize::new(0));

    let warm_cache = ChildSpec::new("warm-cache", move |ctx| {
        let warm_cache_attempts = Arc::clone(&warm_cache_attempts);
        async move {
            let attempt = warm_cache_attempts.fetch_add(1, Ordering::SeqCst);
            println!(
                "warm-cache started in generation {} (attempt {})",
                ctx.generation,
                attempt + 1
            );

            if attempt == 0 {
                sleep(Duration::from_millis(50)).await;
                println!("warm-cache failed during initial generation");
                return Err(example_error("cache priming failed"));
            }

            ctx.token.cancelled().await;
            println!("warm-cache observed shutdown");
            Ok(())
        }
    })
    .restart(Restart::Transient)
    .restart_intensity(RestartIntensity {
        max_restarts: 1,
        within: Duration::from_secs(1),
        backoff: BackoffPolicy::Fixed(Duration::from_millis(100)),
    });

    let metrics = ChildSpec::new("metrics", |ctx| async move {
        println!("metrics started in generation {}", ctx.generation);
        ctx.token.cancelled().await;
        println!("metrics observed shutdown");
        Ok(())
    });

    // Supervisor default: children do not get any restart budget unless they override it.
    let supervisor = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 0,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(warm_cache)
        .child(metrics)
        .build()?;

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    loop {
        let event = timeout(Duration::from_secs(2), events.recv()).await??;
        println!("event: {event:?}");

        match event {
            SupervisorEvent::ChildRestartScheduled {
                id,
                generation,
                delay,
            } if id == "warm-cache" => {
                println!(
                    "warm-cache generation {} is allowed one delayed restart: {delay:?}",
                    generation
                );
            }
            SupervisorEvent::ChildRestarted {
                id,
                old_generation,
                new_generation,
            } if id == "warm-cache" => {
                assert_eq!(old_generation, 0);
                assert_eq!(new_generation, 1);
                break;
            }
            SupervisorEvent::RestartIntensityExceeded => {
                return Err(std::io::Error::other(
                    "unexpected restart intensity failure in example",
                )
                .into());
            }
            _ => {}
        }
    }

    handle.shutdown();
    let exit = handle.wait().await?;
    assert_eq!(exit, SupervisorExit::Shutdown);
    println!("supervisor exited with {exit:?}");

    Ok(())
}
