use std::{sync::Arc, time::Duration};

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
    handle.shutdown_and_wait().await.unwrap();
}
