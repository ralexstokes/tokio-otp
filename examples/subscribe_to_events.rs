use tokio::time::{Duration, sleep};
use tokio_supervisor::{
    ChildSpec, ExitStatusView, SupervisorBuilder, SupervisorEvent, SupervisorExit,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            println!("worker started");
            ctx.token.cancelled().await;
            println!("worker shutting down");
            Ok(())
        }))
        .build()?;

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    let observer = tokio::spawn(async move {
        loop {
            let event = events.recv().await?;
            if matches!(event, SupervisorEvent::SupervisorStopped) {
                print_event(&event, 0);
                break;
            }

            print_event(&event, 0);
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

fn print_event(event: &SupervisorEvent, depth: usize) {
    let indent = "  ".repeat(depth);

    match event {
        SupervisorEvent::SupervisorStarted => println!("{indent}supervisor started"),
        SupervisorEvent::SupervisorStopping => println!("{indent}supervisor stopping"),
        SupervisorEvent::SupervisorStopped => {
            println!("{indent}supervisor stopped");
        }
        SupervisorEvent::Nested {
            id,
            generation,
            event,
        } => {
            println!("{indent}nested supervisor child: {id} generation={generation}");
            print_event(event, depth + 1);
        }
        SupervisorEvent::ChildStarted { id, generation } => {
            println!("{indent}child started: {id} generation={generation}");
        }
        SupervisorEvent::ChildExited {
            id,
            generation,
            status,
        } => match status {
            ExitStatusView::Completed => {
                println!("{indent}child exited cleanly: {id} generation={generation}");
            }
            ExitStatusView::Failed(err) => {
                println!("{indent}child failed: {id} generation={generation} error={err}");
            }
            ExitStatusView::Panicked => {
                println!("{indent}child panicked: {id} generation={generation}");
            }
            ExitStatusView::Aborted => {
                println!("{indent}child aborted: {id} generation={generation}");
            }
        },
        SupervisorEvent::ChildRestartScheduled {
            id,
            generation,
            delay,
        } => {
            println!(
                "{indent}child restart scheduled: {id} generation={generation} delay={delay:?}"
            );
        }
        SupervisorEvent::ChildRestarted {
            id,
            old_generation,
            new_generation,
        } => {
            println!("{indent}child restarted: {id} {old_generation}->{new_generation}");
        }
        SupervisorEvent::GroupRestartScheduled { delay } => {
            println!("{indent}group restart scheduled: delay={delay:?}");
        }
        SupervisorEvent::RestartIntensityExceeded => {
            println!("{indent}restart intensity exceeded");
        }
    }
}
