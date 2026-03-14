//! Web-based dashboard for visualizing live `tokio-otp` supervisor state.
//!
//! `tokio-otp-console` hosts an axum web server with WebSocket streaming and
//! an embedded single-file HTML/JS/CSS frontend. It renders supervision trees,
//! child states, events, and summary stats in real time.
//!
//! # Usage
//!
//! ```no_run
//! use tokio_otp_console::Console;
//! # use tokio::sync::{broadcast, watch};
//! # use tokio_supervisor::{SupervisorSnapshot, SupervisorEvent, SupervisorStateView, Strategy};
//! # let snapshot = SupervisorSnapshot {
//! #     state: SupervisorStateView::Running,
//! #     last_exit: None,
//! #     strategy: Strategy::OneForOne,
//! #     children: vec![],
//! # };
//! # let (snap_tx, snap_rx) = watch::channel(snapshot);
//! # let (evt_tx, _) = broadcast::channel(64);
//!
//! # #[tokio::main]
//! # async fn main() {
//! let handle = Console::builder()
//!     .snapshots(snap_rx)
//!     .events(evt_tx)
//!     .build()
//!     .spawn()
//!     .await
//!     .expect("failed to start console");
//!
//! println!("Console at http://{}", handle.local_addr());
//! # }
//! ```

mod server;
mod ws;

use std::net::SocketAddr;

use tokio::sync::{broadcast, watch};
use tokio_supervisor::{SupervisorEvent, SupervisorSnapshot};

/// Builder for configuring a [`Console`] server.
pub struct ConsoleBuilder {
    snapshots: Option<watch::Receiver<SupervisorSnapshot>>,
    events: Option<broadcast::Sender<SupervisorEvent>>,
    bind: SocketAddr,
}

impl ConsoleBuilder {
    fn new() -> Self {
        Self {
            snapshots: None,
            events: None,
            bind: ([127, 0, 0, 1], 9100).into(),
        }
    }

    /// Sets the snapshot watch receiver.
    pub fn snapshots(mut self, rx: watch::Receiver<SupervisorSnapshot>) -> Self {
        self.snapshots = Some(rx);
        self
    }

    /// Sets the event broadcast sender. Each WebSocket connection will call
    /// `subscribe()` to get its own receiver.
    pub fn events(mut self, tx: broadcast::Sender<SupervisorEvent>) -> Self {
        self.events = Some(tx);
        self
    }

    /// Sets the bind address. Defaults to `127.0.0.1:9100`.
    pub fn bind(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.bind = addr.into();
        self
    }

    /// Validates the builder and returns a [`Console`].
    ///
    /// # Panics
    ///
    /// Panics if `snapshots` or `events` have not been set.
    pub fn build(self) -> Console {
        Console {
            snapshots: self.snapshots.expect("ConsoleBuilder: snapshots required"),
            events: self.events.expect("ConsoleBuilder: events required"),
            bind: self.bind,
        }
    }
}

/// A configured console server ready to start.
pub struct Console {
    snapshots: watch::Receiver<SupervisorSnapshot>,
    events: broadcast::Sender<SupervisorEvent>,
    bind: SocketAddr,
}

impl Console {
    /// Returns a new [`ConsoleBuilder`].
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::new()
    }

    /// Binds the listener and spawns the server in the background.
    ///
    /// Returns a [`ConsoleHandle`] that can be used to query the local address
    /// or shut the server down.
    pub async fn spawn(self) -> std::io::Result<ConsoleHandle> {
        server::spawn(self.snapshots, self.events, self.bind).await
    }

    /// Runs the server on the current task until shut down.
    pub async fn run(self) -> std::io::Result<()> {
        server::run(self.snapshots, self.events, self.bind).await
    }
}

/// Handle to a running console server.
pub struct ConsoleHandle {
    shutdown_tx: watch::Sender<bool>,
    local_addr: SocketAddr,
}

impl ConsoleHandle {
    /// Returns the address the server is listening on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signals the server to shut down.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}
