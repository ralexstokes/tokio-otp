#![allow(dead_code)]

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{sync::mpsc, time::timeout};
use tokio_supervisor::BoxError;

pub const EVENT_TIMEOUT: Duration = Duration::from_secs(2);
pub const QUIET_TIMEOUT: Duration = Duration::from_millis(150);
pub const SHORT_GRACE: Duration = Duration::from_millis(50);

pub fn test_error(message: &'static str) -> BoxError {
    Box::new(std::io::Error::other(message))
}

pub async fn recv_event<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T
where
    T: Debug,
{
    timeout(EVENT_TIMEOUT, rx.recv())
        .await
        .expect("timed out waiting for channel event")
        .expect("channel closed before expected event arrived")
}

pub async fn recv_n<T>(rx: &mut mpsc::UnboundedReceiver<T>, n: usize) -> Vec<T>
where
    T: Debug,
{
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        items.push(recv_event(rx).await);
    }
    items
}

pub async fn assert_no_event<T>(rx: &mut mpsc::UnboundedReceiver<T>)
where
    T: Debug,
{
    if let Ok(Some(value)) = timeout(QUIET_TIMEOUT, rx.recv()).await {
        panic!("unexpected event arrived: {value:?}");
    }
}

#[derive(Clone, Debug)]
pub struct LiveFlag(Arc<AtomicBool>);

impl LiveFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn is_live(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn guard(&self) -> LiveGuard {
        self.0.store(true, Ordering::SeqCst);
        LiveGuard(self.0.clone())
    }
}

pub struct LiveGuard(Arc<AtomicBool>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
