use tokio::time::{Duration, sleep};
use tokio_supervisor::{
    ChildResult, ChildSpec, ExitStatusView, SupervisorBuilder, SupervisorEvent, SupervisorExit,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            println!("worker started");
            ctx.token.cancelled().await;
            println!("worker shutting down");
            ChildResult::Completed
        }))
        .build()?;

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    let observer = tokio::spawn(async move {
        loop {
            let event = events.recv().await?;
            match event {
                SupervisorEvent::SupervisorStarted => println!("supervisor started"),
                SupervisorEvent::SupervisorStopping => println!("supervisor stopping"),
                SupervisorEvent::SupervisorStopped => {
                    println!("supervisor stopped");
                    break;
                }
                SupervisorEvent::ChildStarted { id, generation } => {
                    println!("child started: {id} generation={generation}");
                }
                SupervisorEvent::ChildExited {
                    id,
                    generation,
                    status,
                } => match status {
                    ExitStatusView::Completed => {
                        println!("child exited cleanly: {id} generation={generation}");
                    }
                    ExitStatusView::Failed(err) => {
                        println!("child failed: {id} generation={generation} error={err}");
                    }
                    ExitStatusView::Panicked => {
                        println!("child panicked: {id} generation={generation}");
                    }
                    ExitStatusView::Aborted => {
                        println!("child aborted: {id} generation={generation}");
                    }
                },
                SupervisorEvent::ChildRestartScheduled {
                    id,
                    generation,
                    delay,
                } => {
                    println!(
                        "child restart scheduled: {id} generation={generation} delay={delay:?}"
                    );
                }
                SupervisorEvent::ChildRestarted {
                    id,
                    old_generation,
                    new_generation,
                } => {
                    println!("child restarted: {id} {old_generation}->{new_generation}");
                }
                SupervisorEvent::GroupRestartScheduled { delay } => {
                    println!("group restart scheduled: delay={delay:?}");
                }
                SupervisorEvent::RestartIntensityExceeded => {
                    println!("restart intensity exceeded");
                }
            }
        }

        Ok::<(), tokio::sync::broadcast::error::RecvError>(())
    });

    sleep(Duration::from_millis(200)).await;
    handle.shutdown();

    let exit = handle.wait().await?;
    assert_eq!(exit, SupervisorExit::Shutdown);
    observer.await??;

    Ok(())
}
