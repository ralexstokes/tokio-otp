use std::{sync::Arc, time::Duration};

use tokio::sync::{Mutex, Notify};
use tokio_otp::prelude::*;

#[derive(Clone)]
struct Probe {
    name: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
    release: Option<Arc<Notify>>,
}

impl Actor for Probe {
    type Msg = &'static str;

    async fn on_start(&mut self, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        self.order.lock().await.push(self.name);
        if let Some(release) = &self.release {
            release.notified().await;
        }
        if self.name == "first" {
            ctx.continue_with("continue");
        }
        Ok(())
    }

    async fn handle(&mut self, message: Self::Msg, _ctx: &ActorContext<Self::Msg>) -> ActorResult {
        self.order.lock().await.push(message);
        Ok(())
    }
}

#[tokio::test]
async fn actors_gate_sequential_start_on_on_start_and_run_continuations_first() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Notify::new());
    let mut graph = GraphBuilder::new();
    let first = graph.add(Probe {
        name: "first",
        order: Arc::clone(&order),
        release: Some(Arc::clone(&release)),
    });
    graph.add(Probe {
        name: "second",
        order: Arc::clone(&order),
        release: None,
    });

    let handle = Runtime::builder()
        .graph(graph.build().unwrap())
        .start_mode(StartMode::Sequential)
        .build()
        .unwrap()
        .spawn();

    first.send("mailbox").await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(&*order.lock().await, &["first"]);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if order.lock().await.len() >= 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let observed = order.lock().await.clone();
    assert_eq!(observed[0], "first");
    assert!(observed.contains(&"second"));
    let continuation = observed
        .iter()
        .position(|item| *item == "continue")
        .unwrap();
    let mailbox = observed.iter().position(|item| *item == "mailbox").unwrap();
    assert!(continuation < mailbox);
    handle.shutdown_and_wait().await.unwrap();
}
