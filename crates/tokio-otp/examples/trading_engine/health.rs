use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;
use tokio_otp::prelude::*;

use crate::messages::{ControlMsg, HealthMsg};

const BREAKER_WINDOW: Duration = Duration::from_secs(10);
const BREAKER_THRESHOLD: usize = 4;

#[derive(Clone)]
pub struct Health {
    restarts: VecDeque<Instant>,
    tripped: bool,
    control: ActorRef<ControlMsg>,
}

impl Health {
    pub fn new(control: ActorRef<ControlMsg>) -> Self {
        Self {
            restarts: VecDeque::new(),
            tripped: false,
            control,
        }
    }
}

impl Actor for Health {
    type Msg = HealthMsg;

    async fn handle(&mut self, message: HealthMsg, _ctx: &ActorContext<HealthMsg>) -> ActorResult {
        match message {
            HealthMsg::RestartsObserved { count } => {
                tracing::warn!(count, "venue subtree restarts observed");
                let now = Instant::now();
                // A single observation may cover several restarts when
                // snapshot updates were conflated; count each one.
                for _ in 0..count {
                    self.restarts.push_back(now);
                }
                while self
                    .restarts
                    .front()
                    .is_some_and(|at| now.duration_since(*at) > BREAKER_WINDOW)
                {
                    self.restarts.pop_front();
                }
                if !self.tripped && self.restarts.len() >= BREAKER_THRESHOLD {
                    self.tripped = true;
                    // Control closes the shared intake gate before awaiting
                    // cancellations. Even if this bounded wait expires, the
                    // already-accepted kill switch still takes effect.
                    let _ = tokio::time::timeout(
                        Duration::from_millis(500),
                        self.control.call(|reply| ControlMsg::KillSwitch { reply }),
                    )
                    .await;
                }
            }
            HealthMsg::ResetBreaker => {
                self.restarts.clear();
                self.tripped = false;
            }
            HealthMsg::Tripped { reply } => reply.send(self.tripped),
        }
        Ok(())
    }
}
