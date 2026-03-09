use tokio::time::{Duration, sleep};
use tokio_supervisor::{ChildResult, ChildSpec, SupervisorBuilder, SupervisorExit};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("http-server", |ctx| async move {
            println!("http-server started");

            loop {
                tokio::select! {
                    _ = ctx.token.cancelled() => {
                        println!("http-server received cancellation");
                        return ChildResult::Completed;
                    }
                    _ = sleep(Duration::from_millis(75)) => {
                        println!("http-server heartbeat");
                    }
                }
            }
        }))
        .build()?;

    let handle = supervisor.spawn();

    let app_shutdown = CancellationToken::new();
    let trigger = tokio::spawn({
        let app_shutdown = app_shutdown.clone();
        async move {
            sleep(Duration::from_millis(250)).await;
            println!("application shutdown requested");
            app_shutdown.cancel();
        }
    });

    app_shutdown.cancelled().await;
    handle.shutdown();

    let exit = handle.wait().await?;
    assert_eq!(exit, SupervisorExit::Shutdown);
    println!("supervisor exited with {exit:?}");

    trigger.await?;
    Ok(())
}
