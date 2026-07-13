use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio_otp::prelude::*;

use crate::messages::{
    CALL_DEADLINE, CancelOutcome, GatewayMsg, LedgerMsg, OrderKey, PlaceOutcome, QueryOutcome,
    RouterMsg, SubmitResult, VenueId,
};

#[derive(Clone, Debug)]
struct OrderIntent {
    venue: VenueId,
    symbol: &'static str,
    qty: i64,
    unknown: bool,
}

#[derive(Clone)]
pub struct OrderRouter {
    sequence: u64,
    intents: HashMap<OrderKey, OrderIntent>,
    gateways: HashMap<VenueId, ActorRef<GatewayMsg>>,
    ledger: ActorRef<LedgerMsg>,
    pub intake_gate: Arc<AtomicBool>,
}

impl OrderRouter {
    pub fn new(
        gateways: HashMap<VenueId, ActorRef<GatewayMsg>>,
        ledger: ActorRef<LedgerMsg>,
        intake_gate: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sequence: 0,
            intents: HashMap::new(),
            gateways,
            ledger,
            intake_gate,
        }
    }
}

impl Actor for OrderRouter {
    type Msg = RouterMsg;

    async fn handle(&mut self, message: RouterMsg, _ctx: &ActorContext<RouterMsg>) -> ActorResult {
        match message {
            RouterMsg::Submit {
                symbol,
                qty,
                venue,
                reply,
            } => {
                if !self.intake_gate.load(Ordering::Acquire) {
                    reply.send(SubmitResult::IntakeClosed);
                    return Ok(());
                }
                self.sequence += 1;
                let key = format!("ord-{}", self.sequence);
                let gateway = self.gateways.get(venue).expect("known venue");
                let result = tokio::time::timeout(
                    CALL_DEADLINE,
                    gateway.call(|reply| GatewayMsg::Place {
                        key: key.clone(),
                        symbol,
                        qty,
                        reply,
                    }),
                )
                .await;
                match result {
                    Ok(Ok(PlaceOutcome::Accepted { .. })) => {
                        self.intents.insert(
                            key.clone(),
                            OrderIntent {
                                venue,
                                symbol,
                                qty,
                                unknown: false,
                            },
                        );
                        reply.send(SubmitResult::Placed(key));
                    }
                    Err(_) => {
                        self.intents.insert(
                            key.clone(),
                            OrderIntent {
                                venue,
                                symbol,
                                qty,
                                unknown: true,
                            },
                        );
                        reply.send(SubmitResult::Unknown(key));
                    }
                    Ok(Err(_)) => reply.send(SubmitResult::Rejected),
                }
            }
            RouterMsg::Cancel { key, reply } => {
                let Some(intent) = self.intents.get(&key) else {
                    reply.send(CancelOutcome::NotFound);
                    return Ok(());
                };
                let gateway = self.gateways.get(intent.venue).expect("known venue");
                let outcome = match tokio::time::timeout(
                    CALL_DEADLINE,
                    gateway.call(|reply| GatewayMsg::Cancel {
                        key: key.clone(),
                        reply,
                    }),
                )
                .await
                {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(_)) | Err(_) => CancelOutcome::Unknown,
                };
                reply.send(outcome);
            }
            RouterMsg::ReconcileAll { reply } => {
                let unknown = self
                    .intents
                    .iter()
                    .filter(|(_, intent)| intent.unknown)
                    .map(|(key, intent)| (key.clone(), intent.clone()))
                    .collect::<Vec<_>>();
                let mut resolved = 0;
                for (key, intent) in unknown {
                    let gateway = self.gateways.get(intent.venue).expect("known venue");
                    let query = tokio::time::timeout(
                        CALL_DEADLINE,
                        gateway.call(|reply| GatewayMsg::Query {
                            key: key.clone(),
                            reply,
                        }),
                    )
                    .await;
                    match query {
                        Ok(Ok(QueryOutcome::Found(_))) => {
                            self.intents.get_mut(&key).expect("known intent").unknown = false;
                            self.ledger
                                .send(LedgerMsg::Ack {
                                    key: key.clone(),
                                    venue: intent.venue,
                                })
                                .await?;
                            resolved += 1;
                        }
                        Ok(Ok(QueryOutcome::NotFound)) => {
                            let placed = tokio::time::timeout(
                                CALL_DEADLINE,
                                gateway.call(|reply| GatewayMsg::Place {
                                    key: key.clone(),
                                    symbol: intent.symbol,
                                    qty: intent.qty,
                                    reply,
                                }),
                            )
                            .await;
                            if matches!(placed, Ok(Ok(PlaceOutcome::Accepted { .. }))) {
                                self.intents.get_mut(&key).expect("known intent").unknown = false;
                                resolved += 1;
                            }
                        }
                        Ok(Err(_)) | Err(_) => {}
                    }
                }
                reply.send(resolved);
            }
            RouterMsg::InFlight { reply } => reply.send(
                self.intents
                    .values()
                    .filter(|intent| intent.unknown)
                    .count(),
            ),
        }
        Ok(())
    }
}
