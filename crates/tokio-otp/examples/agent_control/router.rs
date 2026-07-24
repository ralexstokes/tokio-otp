//! Session router: the single writer for dynamic session membership.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use tokio_otp::{
    Actor, ActorContext, ActorRef, ActorResult, ControlError, DynamicActorOptions, RuntimeHandle,
    prelude::Continue,
};

use crate::{
    messages::{
        BudgetMsg, ChatId, GuardMsg, JournalMsg, OutboundMsg, PHASE_TIMEOUT, PendingInput,
        ProgressMsg, Proof, RouterMsg, SessionMsg, ToolHostMsg,
    },
    model::ModelClient,
    session::SessionFactory,
};

enum SessionSlot {
    Active {
        actor: ActorRef<SessionMsg>,
        generation: u64,
    },
    Evicting {
        buffered: Vec<PendingInput>,
    },
}

pub struct Router {
    sessions_handle: Option<RuntimeHandle>,
    sessions: HashMap<ChatId, SessionSlot>,
    journal: ActorRef<JournalMsg>,
    budget: ActorRef<BudgetMsg>,
    tool_host: ActorRef<ToolHostMsg>,
    guard: ActorRef<GuardMsg>,
    outbound: ActorRef<OutboundMsg>,
    progress: ActorRef<ProgressMsg>,
    gate: Arc<AtomicBool>,
    model: Arc<dyn ModelClient>,
    task_sequence: Arc<AtomicU64>,
    generation_sequence: Arc<AtomicU64>,
    proof: Proof,
}

impl Router {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        journal: ActorRef<JournalMsg>,
        budget: ActorRef<BudgetMsg>,
        tool_host: ActorRef<ToolHostMsg>,
        guard: ActorRef<GuardMsg>,
        outbound: ActorRef<OutboundMsg>,
        progress: ActorRef<ProgressMsg>,
        gate: Arc<AtomicBool>,
        model: Arc<dyn ModelClient>,
        proof: Proof,
    ) -> Self {
        Self {
            sessions_handle: None,
            sessions: HashMap::new(),
            journal,
            budget,
            tool_host,
            guard,
            outbound,
            progress,
            gate,
            model,
            task_sequence: Arc::new(AtomicU64::new(0)),
            generation_sequence: Arc::new(AtomicU64::new(0)),
            proof,
        }
    }

    async fn add_session(
        &mut self,
        chat: ChatId,
        ctx: &ActorContext<RouterMsg>,
    ) -> Result<ActorRef<SessionMsg>, tokio_otp::ControlError> {
        let sessions = self
            .sessions_handle
            .as_ref()
            .expect("router must be bound before chat traffic")
            .clone();
        let actor = sessions
            .add_actor(
                format!("session:{chat}"),
                SessionFactory {
                    chat,
                    journal: self.journal.clone(),
                    budget: self.budget.clone(),
                    tool_host: self.tool_host.clone(),
                    guard: self.guard.clone(),
                    outbound: self.outbound.clone(),
                    progress: self.progress.clone(),
                    router: ctx.myself(),
                    sessions: sessions.clone(),
                    gate: self.gate.clone(),
                    model: self.model.clone(),
                    task_sequence: self.task_sequence.clone(),
                    generation_sequence: self.generation_sequence.clone(),
                    proof: self.proof.clone(),
                },
                DynamicActorOptions::default(),
            )
            .await?;
        let generation = self.generation_sequence.load(Ordering::Relaxed);
        self.sessions.insert(
            chat,
            SessionSlot::Active {
                actor: actor.clone(),
                generation,
            },
        );
        Ok(actor)
    }

    fn pipeline_remove(&self, chat: ChatId, ctx: &ActorContext<RouterMsg>) {
        let sessions = self.sessions_handle.as_ref().expect("router bound").clone();
        ctx.step(
            PHASE_TIMEOUT,
            async move {
                // Removal is pipelined so an idle eviction never head-of-line
                // blocks unrelated routing work (the same hazard documented for
                // the trading example's order router).
                sessions.remove_child(format!("session:{chat}")).await
            },
            move |outcome| {
                if matches!(
                    outcome,
                    Ok(Ok(()))
                        | Ok(Err(ControlError::UnknownChildId(_)))
                        | Ok(Err(ControlError::ShutdownTimedOut(_)))
                ) {
                    RouterMsg::Removed { chat }
                } else {
                    // A step deadline abandons only our wait. The supervisor
                    // may still be removing this id, so reconcile before a
                    // buffered message is allowed to re-add it.
                    RouterMsg::RetryRemove { chat }
                }
            },
        );
    }
}

impl Actor for Router {
    type Msg = RouterMsg;

    async fn handle(&mut self, message: Self::Msg, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        match message {
            RouterMsg::Bind { sessions } => self.sessions_handle = Some(sessions),
            RouterMsg::UserMessage {
                envelope,
                chat,
                text,
            } => {
                let input = PendingInput { envelope, text };
                match self.sessions.get_mut(chat) {
                    Some(SessionSlot::Active { actor, .. }) => {
                        actor
                            .send(SessionMsg::UserMessage {
                                envelope: input.envelope,
                                text: input.text,
                            })
                            .await?;
                    }
                    Some(SessionSlot::Evicting { buffered }) => {
                        buffered.push(input);
                        self.proof
                            .lock()
                            .expect("proof lock poisoned")
                            .evict_buffered += 1;
                    }
                    None => {
                        let actor = self.add_session(chat, ctx).await?;
                        actor
                            .send(SessionMsg::UserMessage {
                                envelope: input.envelope,
                                text: input.text,
                            })
                            .await?;
                    }
                }
            }
            RouterMsg::Evict { chat, generation } => {
                let should_remove = matches!(
                    self.sessions.get(chat),
                    Some(SessionSlot::Active { generation: active, .. }) if *active == generation
                );
                if should_remove {
                    self.sessions.insert(
                        chat,
                        SessionSlot::Evicting {
                            buffered: Vec::new(),
                        },
                    );
                    self.pipeline_remove(chat, ctx);
                }
            }
            RouterMsg::Removed { chat } => {
                let buffered = match self.sessions.remove(chat) {
                    Some(SessionSlot::Evicting { buffered }) => buffered,
                    Some(other) => {
                        self.sessions.insert(chat, other);
                        Vec::new()
                    }
                    None => Vec::new(),
                };
                if !buffered.is_empty() {
                    let actor = self.add_session(chat, ctx).await?;
                    for input in buffered {
                        actor
                            .send(SessionMsg::UserMessage {
                                envelope: input.envelope,
                                text: input.text,
                            })
                            .await?;
                    }
                }
            }
            RouterMsg::RetryRemove { chat } => {
                if matches!(self.sessions.get(chat), Some(SessionSlot::Evicting { .. })) {
                    self.pipeline_remove(chat, ctx);
                }
            }
            RouterMsg::PauseChanged { paused } => {
                for slot in self.sessions.values() {
                    if let SessionSlot::Active { actor, .. } = slot {
                        let _ = actor.send(SessionMsg::PauseChanged { paused }).await;
                    }
                }
            }
            RouterMsg::Stop { chat } => {
                if let Some(SessionSlot::Active { actor, .. }) = self.sessions.get(chat) {
                    actor.send(SessionMsg::Stop).await?;
                }
            }
        }
        Ok(Continue)
    }
}
