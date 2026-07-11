use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::{Mutex, Notify};
use tokio_supervisor::{ChildSpec, RestartPolicy, StartMode, Strategy, SupervisorBuilder};

#[tokio::test]
async fn sequential_start_waits_for_explicit_readiness() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Notify::new());

    let first = ChildSpec::new("first", {
        let order = Arc::clone(&order);
        let release = Arc::clone(&release);
        move |ctx| {
            let order = Arc::clone(&order);
            let release = Arc::clone(&release);
            async move {
                order.lock().await.push("first");
                release.notified().await;
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let second = ChildSpec::new("second", {
        let order = Arc::clone(&order);
        move |ctx| {
            let order = Arc::clone(&order);
            async move {
                order.lock().await.push("second");
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();

    let handle = SupervisorBuilder::new()
        .start_mode(StartMode::Sequential)
        .child(first)
        .child(second)
        .build()
        .unwrap()
        .spawn();

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(&*order.lock().await, &["first"]);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*order.lock().await, &["first", "second"]);
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn concurrent_start_does_not_wait_for_readiness() {
    let second_started = Arc::new(Notify::new());
    let first_release = Arc::new(Notify::new());

    let first = ChildSpec::new("first", {
        let first_release = Arc::clone(&first_release);
        move |ctx| {
            let first_release = Arc::clone(&first_release);
            async move {
                first_release.notified().await;
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let second = ChildSpec::new("second", {
        let second_started = Arc::clone(&second_started);
        move |ctx| {
            let second_started = Arc::clone(&second_started);
            async move {
                second_started.notify_one();
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();

    let handle = SupervisorBuilder::new()
        .child(first)
        .child(second)
        .build()
        .unwrap()
        .spawn();

    tokio::time::timeout(Duration::from_secs(1), second_started.notified())
        .await
        .unwrap();
    first_release.notify_one();
    handle.wait_started().await.unwrap();
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn one_for_all_restart_preserves_sequential_readiness_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(Notify::new());
    let release_restart = Arc::new(Notify::new());

    let first = ChildSpec::new("first", {
        let order = Arc::clone(&order);
        let fail = Arc::clone(&fail);
        let release_restart = Arc::clone(&release_restart);
        move |ctx| {
            let order = Arc::clone(&order);
            let fail = Arc::clone(&fail);
            let release_restart = Arc::clone(&release_restart);
            async move {
                order.lock().await.push(("first", ctx.generation()));
                if ctx.generation() > 0 {
                    release_restart.notified().await;
                }
                ctx.mark_ready();
                if ctx.generation() == 0 {
                    fail.notified().await;
                    Err(std::io::Error::other("restart").into())
                } else {
                    ctx.shutdown_token().cancelled().await;
                    Ok(())
                }
            }
        }
    })
    .wait_for_ready();
    let second = ChildSpec::new("second", {
        let order = Arc::clone(&order);
        move |ctx| {
            let order = Arc::clone(&order);
            async move {
                order.lock().await.push(("second", ctx.generation()));
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();

    let handle = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .start_mode(StartMode::Sequential)
        .child(first)
        .child(second)
        .build()
        .unwrap()
        .spawn();
    handle.wait_started().await.unwrap();
    fail.notify_one();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if order.lock().await.contains(&("first", 1)) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!order.lock().await.contains(&("second", 1)));
    release_restart.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if order.lock().await.contains(&("second", 1)) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn startup_failure_leaves_later_sequential_children_unstarted() {
    let later_started = Arc::new(Notify::new());
    let failed = ChildSpec::new("failed", |_| async {
        Err(std::io::Error::other("init failed").into())
    })
    .restart(RestartPolicy::Never)
    .wait_for_ready();
    let later = ChildSpec::new("later", {
        let later_started = Arc::clone(&later_started);
        move |ctx| {
            let later_started = Arc::clone(&later_started);
            async move {
                later_started.notify_one();
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();

    let handle = SupervisorBuilder::new()
        .start_mode(StartMode::Sequential)
        .child(failed)
        .child(later)
        .build()
        .unwrap()
        .spawn();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), later_started.notified())
            .await
            .is_err()
    );
    assert!(matches!(
        handle.wait_started().await,
        Err(tokio_supervisor::SupervisorError::StartupAborted(_))
    ));
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn sequential_start_resumes_after_pre_ready_restart() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let later_started = Arc::new(Notify::new());
    let flaky = ChildSpec::new("flaky", {
        let attempts = Arc::clone(&attempts);
        move |ctx| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    return Err(std::io::Error::other("retry init").into());
                }
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let later = ChildSpec::new("later", {
        let later_started = Arc::clone(&later_started);
        move |ctx| {
            let later_started = Arc::clone(&later_started);
            async move {
                later_started.notify_one();
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let handle = SupervisorBuilder::new()
        .start_mode(StartMode::Sequential)
        .child(flaky)
        .child(later)
        .build()
        .unwrap()
        .spawn();
    tokio::time::timeout(Duration::from_secs(1), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    tokio::time::timeout(Duration::from_secs(1), later_started.notified())
        .await
        .unwrap();
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn wait_started_accepts_an_immediate_child_that_already_completed() {
    let handle = SupervisorBuilder::new()
        .child(ChildSpec::new("oneshot", |_| async { Ok(()) }).restart(RestartPolicy::Never))
        .build()
        .unwrap()
        .spawn();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tokio::time::timeout(Duration::from_secs(1), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn dynamic_gated_child_waits_for_readiness_in_sequential_mode() {
    let release = Arc::new(Notify::new());
    let handle = SupervisorBuilder::new()
        .start_mode(StartMode::Sequential)
        .build()
        .unwrap()
        .spawn();
    let child = ChildSpec::new("dynamic", {
        let release = Arc::clone(&release);
        move |ctx| {
            let release = Arc::clone(&release);
            async move {
                release.notified().await;
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let add = tokio::spawn({
        let handle = handle.clone();
        async move { handle.add_child(child).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!add.is_finished());
    release.notify_one();
    add.await.unwrap().unwrap();
    handle.wait_started().await.unwrap();
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn nested_supervisor_gates_later_parent_siblings() {
    let release = Arc::new(Notify::new());
    let later_started = Arc::new(Notify::new());
    let nested = SupervisorBuilder::new()
        .child(
            ChildSpec::new("nested-child", {
                let release = Arc::clone(&release);
                move |ctx| {
                    let release = Arc::clone(&release);
                    async move {
                        release.notified().await;
                        ctx.mark_ready();
                        ctx.shutdown_token().cancelled().await;
                        Ok(())
                    }
                }
            })
            .wait_for_ready(),
        )
        .build()
        .unwrap();
    let later = ChildSpec::new("later", {
        let later_started = Arc::clone(&later_started);
        move |ctx| {
            let later_started = Arc::clone(&later_started);
            async move {
                later_started.notify_one();
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let handle = SupervisorBuilder::new()
        .start_mode(StartMode::Sequential)
        .supervisor("nested", nested)
        .child(later)
        .build()
        .unwrap()
        .spawn();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), later_started.notified())
            .await
            .is_err()
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    handle.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn rest_for_one_restart_preserves_sequential_readiness_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(Notify::new());
    let release_restart = Arc::new(Notify::new());
    let anchor = ChildSpec::new("anchor", {
        let order = Arc::clone(&order);
        move |ctx| {
            let order = Arc::clone(&order);
            async move {
                order.lock().await.push(("anchor", ctx.generation()));
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let middle = ChildSpec::new("middle", {
        let order = Arc::clone(&order);
        let fail = Arc::clone(&fail);
        let release_restart = Arc::clone(&release_restart);
        move |ctx| {
            let order = Arc::clone(&order);
            let fail = Arc::clone(&fail);
            let release_restart = Arc::clone(&release_restart);
            async move {
                order.lock().await.push(("middle", ctx.generation()));
                if ctx.generation() > 0 {
                    release_restart.notified().await;
                }
                ctx.mark_ready();
                if ctx.generation() == 0 {
                    fail.notified().await;
                    Err(std::io::Error::other("restart suffix").into())
                } else {
                    ctx.shutdown_token().cancelled().await;
                    Ok(())
                }
            }
        }
    })
    .wait_for_ready();
    let last = ChildSpec::new("last", {
        let order = Arc::clone(&order);
        move |ctx| {
            let order = Arc::clone(&order);
            async move {
                order.lock().await.push(("last", ctx.generation()));
                ctx.mark_ready();
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .wait_for_ready();
    let handle = SupervisorBuilder::new()
        .strategy(Strategy::RestForOne)
        .start_mode(StartMode::Sequential)
        .child(anchor)
        .child(middle)
        .child(last)
        .build()
        .unwrap()
        .spawn();
    handle.wait_started().await.unwrap();
    fail.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if order.lock().await.contains(&("middle", 1)) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!order.lock().await.contains(&("last", 1)));
    assert!(!order.lock().await.contains(&("anchor", 1)));
    release_restart.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if order.lock().await.contains(&("last", 1)) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handle.shutdown_and_wait().await.unwrap();
}
