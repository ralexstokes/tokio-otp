use std::time::Instant;

use tokio_otp::prelude::*;

use crate::{
    messages::{LedgerMsg, LedgerReport},
    telemetry::LatencyRecorder,
};

#[derive(Clone, Default)]
pub struct Ledger {
    report: LedgerReport,
    latency: LatencyRecorder,
}

impl Ledger {
    pub fn new(latency: LatencyRecorder) -> Self {
        Self {
            report: LedgerReport::default(),
            latency,
        }
    }
}

impl Actor for Ledger {
    type Msg = LedgerMsg;

    async fn handle(&mut self, message: LedgerMsg, _ctx: &ActorContext<LedgerMsg>) -> ActorResult {
        match message {
            LedgerMsg::Ack { key, venue } => {
                let _venue = venue;
                self.report.effects.entry(key).or_default().acknowledgements += 1;
            }
            LedgerMsg::Fill {
                key,
                venue,
                qty,
                enqueued_at,
            } => {
                let _venue = venue;
                let _qty = qty;
                self.latency.record(
                    "queue.fill",
                    Instant::now().saturating_duration_since(enqueued_at),
                );
                self.report.effects.entry(key).or_default().fills += 1;
            }
            LedgerMsg::Cancelled { key, venue } => {
                let _venue = venue;
                self.report.effects.entry(key).or_default().cancellations += 1;
            }
            LedgerMsg::Report { reply } => reply.send(self.report.clone()),
        }
        Ok(())
    }

    fn drain_policy(&self) -> DrainPolicy {
        DrainPolicy::Drain
    }
}
