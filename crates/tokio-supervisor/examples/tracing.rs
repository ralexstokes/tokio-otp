use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::time::{Duration, sleep};
use tokio_supervisor::prelude::*;
use tracing_subscriber::fmt::format::FmtSpan;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .compact()
        .init();

    let attempts = Arc::new(AtomicUsize::new(0));
    let nested_attempts = Arc::clone(&attempts);
    let nested = SupervisorBuilder::new()
        .child(
            ChildSpec::new("leaf", move |ctx| {
                let attempts = Arc::clone(&nested_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(std::io::Error::other("fail once").into());
                    }

                    ctx.shutdown_token().cancelled().await;
                    Ok(())
                }
            })
            .restart(Restart::Transient)
            .restart_intensity(RestartIntensity {
                max_restarts: 5,
                within: Duration::from_secs(5),
                backoff: BackoffPolicy::Fixed(Duration::from_millis(100)),
            }),
        )
        .build()?;

    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("anchor", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .child(nested.into_child_spec("nested"))
        .build()?;

    let handle = supervisor.spawn();

    sleep(Duration::from_millis(300)).await;
    handle.shutdown();

    handle.wait().await?;

    Ok(())
}
