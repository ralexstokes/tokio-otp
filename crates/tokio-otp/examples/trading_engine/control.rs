use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio_otp::prelude::*;

use crate::messages::{CALL_DEADLINE, ControlMsg, GatewayMsg};

#[derive(Clone)]
pub struct Control {
    pub gateways: Vec<ActorRef<GatewayMsg>>,
    pub intake_gate: Arc<AtomicBool>,
}

impl Control {
    async fn cancel_gateway(gateway: ActorRef<GatewayMsg>) -> usize {
        tokio::time::timeout(
            CALL_DEADLINE,
            gateway.call(|reply| GatewayMsg::CancelAll { reply }),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
    }

    async fn cancel_all(&self) -> usize {
        assert_eq!(self.gateways.len(), 2, "the example has exactly two venues");
        let (venue_a, venue_b) = tokio::join!(
            Self::cancel_gateway(self.gateways[0].clone()),
            Self::cancel_gateway(self.gateways[1].clone())
        );
        venue_a + venue_b
    }
}

impl Actor for Control {
    type Msg = ControlMsg;

    async fn handle(
        &mut self,
        message: ControlMsg,
        _ctx: &ActorContext<ControlMsg>,
    ) -> ActorResult {
        match message {
            ControlMsg::KillSwitch { reply } => {
                self.intake_gate.store(false, Ordering::Release);
                self.cancel_all().await;
                reply.send(());
            }
            ControlMsg::EmergencyCancelAll { reply } => reply.send(self.cancel_all().await),
        }
        Ok(())
    }
}
