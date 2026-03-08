use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::mpsc;
use tokio_supervisor::{
    ChildResult, ChildSpec, Restart, Strategy, SupervisorBuilder, SupervisorExit,
};

mod common;

#[tokio::test]
async fn transient_child_panic_causes_restart() {
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::new(0));

    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("panic-worker", move |ctx| {
                let attempts = attempts.clone();
                let starts_tx = starts_tx.clone();
                async move {
                    starts_tx
                        .send(ctx.generation)
                        .expect("test receiver dropped");
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("boom");
                    }

                    ctx.token.cancelled().await;
                    ChildResult::Completed
                }
            })
            .restart(Restart::Transient),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_n(&mut starts_rx, 2).await, vec![0, 1]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn one_for_all_panic_restarts_the_whole_group() {
    let (panic_tx, mut panic_rx) = mpsc::unbounded_channel();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::new(0));

    let panic_child = ChildSpec::new("panic-worker", move |ctx| {
        let attempts = attempts.clone();
        let panic_tx = panic_tx.clone();
        async move {
            panic_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("boom");
            }

            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Transient);

    let peer = ChildSpec::new("peer", move |ctx| {
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Permanent);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .child(panic_child)
        .child(peer)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_n(&mut panic_rx, 2).await, vec![0, 1]);
    assert_eq!(common::recv_n(&mut peer_rx, 2).await, vec![0, 1]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}
