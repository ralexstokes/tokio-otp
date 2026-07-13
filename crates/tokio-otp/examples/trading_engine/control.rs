use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio_otp::prelude::*;

use crate::messages::{ControlMsg, GatewayMsg};

const CONTROL_CALL_DEADLINE: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct Control {
    pub gateways: Vec<ActorRef<GatewayMsg>>,
    pub intake_gate: Arc<AtomicBool>,
}

impl Control {
    async fn cancel_all(&self) -> usize {
        let mut total = 0;
        for gateway in &self.gateways {
            if let Ok(Ok(cancelled)) = tokio::time::timeout(
                CONTROL_CALL_DEADLINE,
                gateway.call(|reply| GatewayMsg::CancelAll { reply }),
            )
            .await
            {
                total += cancelled;
            }
        }
        total
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
