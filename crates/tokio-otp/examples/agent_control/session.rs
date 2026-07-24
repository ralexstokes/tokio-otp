//! Dynamic conversation orchestrator and owner of transient role-run children.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use tokio::time::Instant;
use tokio_otp::{
    Actor, ActorContext, ActorOptions, ActorRef, ActorResult, CancellationToken, DrainPolicy,
    DynamicActorOptions, RestartPolicy, RuntimeHandle, TimerRef,
};

use crate::{
    messages::{
        BudgetMsg, ChatId, EVICT_FLUSH, GuardMsg, IDLE_TIMEOUT, JournalEntry, JournalMsg,
        OutboundMsg, PHASE_TIMEOUT, PendingInput, ProgressMsg, Proof, Role, RouterMsg, RunOutput,
        SessionMsg, TYPING_PERIOD, TaskId, ToolHostMsg,
    },
    model::ModelClient,
    run::AgentRunFactory,
};

struct ActiveRun {
    id: String,
    task: TaskId,
    role: Role,
    attempt: u64,
    input: PendingInput,
    cancel: CancellationToken,
    failure_reported: bool,
    retry_after_remove: bool,
}

pub struct Session {
    chat: ChatId,
    generation: u64,
    journal: ActorRef<JournalMsg>,
    budget: ActorRef<BudgetMsg>,
    tool_host: ActorRef<ToolHostMsg>,
    guard: ActorRef<GuardMsg>,
    outbound: ActorRef<OutboundMsg>,
    progress: ActorRef<ProgressMsg>,
    router: ActorRef<RouterMsg>,
    sessions: RuntimeHandle,
    gate: Arc<AtomicBool>,
    model: Arc<dyn ModelClient>,
    task_sequence: Arc<AtomicU64>,
    proof: Proof,
    transcript_len: usize,
    pending: VecDeque<PendingInput>,
    active: Option<ActiveRun>,
    heartbeat: Option<TimerRef>,
    idle_generation: u64,
    evict_requested: bool,
}

#[derive(Clone)]
pub struct SessionFactory {
    pub chat: ChatId,
    pub journal: ActorRef<JournalMsg>,
    pub budget: ActorRef<BudgetMsg>,
    pub tool_host: ActorRef<ToolHostMsg>,
    pub guard: ActorRef<GuardMsg>,
    pub outbound: ActorRef<OutboundMsg>,
    pub progress: ActorRef<ProgressMsg>,
    pub router: ActorRef<RouterMsg>,
    pub sessions: RuntimeHandle,
    pub gate: Arc<AtomicBool>,
    pub model: Arc<dyn ModelClient>,
    pub task_sequence: Arc<AtomicU64>,
    pub generation_sequence: Arc<AtomicU64>,
    pub proof: Proof,
}

impl tokio_otp::ActorFactory for SessionFactory {
    type Actor = Session;

    fn build(&self) -> Self::Actor {
        let generation = self.generation_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        Session {
            chat: self.chat,
            generation,
            journal: self.journal.clone(),
            budget: self.budget.clone(),
            tool_host: self.tool_host.clone(),
            guard: self.guard.clone(),
            outbound: self.outbound.clone(),
            progress: self.progress.clone(),
            router: self.router.clone(),
            sessions: self.sessions.clone(),
            gate: self.gate.clone(),
            model: self.model.clone(),
            task_sequence: self.task_sequence.clone(),
            proof: self.proof.clone(),
            transcript_len: 0,
            pending: VecDeque::new(),
            active: None,
            heartbeat: None,
            idle_generation: 0,
            evict_requested: false,
        }
    }
}

impl Session {
    async fn append(&self, entry: JournalEntry) -> ActorResult {
        self.journal
            .call(PHASE_TIMEOUT, |reply| JournalMsg::Append {
                chat: self.chat,
                entry,
                reply,
            })
            .await?;
        Ok(())
    }

    fn arm_idle(&mut self, ctx: &ActorContext<SessionMsg>) {
        self.idle_generation += 1;
        ctx.state_timeout(
            SessionMsg::IdleSweep {
                generation: self.idle_generation,
            },
            IDLE_TIMEOUT,
        );
    }

    async fn start_run(
        &mut self,
        task: TaskId,
        role: Role,
        attempt: u64,
        input: PendingInput,
        ctx: &ActorContext<SessionMsg>,
    ) -> ActorResult {
        ctx.clear_state_timeout();
        if self.heartbeat.is_none() {
            self.heartbeat = Some(ctx.interval_to(
                &self.progress,
                ProgressMsg::Typing { chat: self.chat },
                TYPING_PERIOD,
            ));
        }
        let cancel = CancellationToken::new();
        let role_name = match role {
            Role::Planner => "planner",
            Role::Engineer => "engineer",
            Role::Reviewer => "reviewer",
        };
        let id = format!("run:{}:{task}:{role_name}", self.chat);
        let run_ref = self
            .sessions
            .add_actor_with_options(
                id.clone(),
                AgentRunFactory {
                    chat: self.chat,
                    task,
                    role,
                    attempt,
                    user_text: input.text.clone(),
                    model: self.model.clone(),
                    journal: self.journal.clone(),
                    budget: self.budget.clone(),
                    tool_host: self.tool_host.clone(),
                    progress: self.progress.clone(),
                    session: ctx.myself(),
                    cancel: cancel.clone(),
                },
                ActorOptions::new(),
                DynamicActorOptions::default().restart(RestartPolicy::Never),
            )
            .await?;
        ctx.watch(&run_ref, move |event| SessionMsg::RunEvent {
            task,
            role,
            event,
        });
        self.active = Some(ActiveRun {
            id,
            task,
            role,
            attempt,
            input,
            cancel,
            failure_reported: false,
            retry_after_remove: false,
        });
        *self
            .proof
            .lock()
            .expect("proof lock poisoned")
            .run_started
            .entry(self.chat)
            .or_default() += 1;
        Ok(())
    }

    async fn start_input(
        &mut self,
        input: PendingInput,
        ctx: &ActorContext<SessionMsg>,
    ) -> ActorResult {
        let task = self.task_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.start_run(task, Role::Planner, 0, input, ctx).await
    }

    fn pipeline_remove(&self, id: String, ctx: &ActorContext<SessionMsg>) {
        let sessions = self.sessions.clone();
        let myself = ctx.myself();
        tokio::spawn(async move {
            let _ = sessions.remove_child(id.clone()).await;
            let _ = myself.send(SessionMsg::RunRemoved { id }).await;
        });
    }

    async fn complete_task(
        &mut self,
        task: TaskId,
        approved: bool,
        ctx: &ActorContext<SessionMsg>,
    ) -> ActorResult {
        let text = format!(
            "task {task} complete (approved={approved}, prior-context={})",
            self.transcript_len.saturating_sub(1)
        );
        self.append(JournalEntry::Reply {
            task,
            text: text.clone(),
        })
        .await?;
        self.outbound
            .send(OutboundMsg::Reply {
                chat: self.chat,
                text,
            })
            .await?;
        if let Some(timer) = self.heartbeat.take() {
            timer.cancel();
        }
        self.proof
            .lock()
            .expect("proof lock poisoned")
            .run_terminal_at
            .insert(self.chat, Instant::now());
        self.active = None;
        self.arm_idle(ctx);
        Ok(())
    }
}

impl Actor for Session {
    type Msg = SessionMsg;

    async fn on_start(&mut self, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        let mut proof = self.proof.lock().expect("proof lock poisoned");
        proof.session_ready_at.insert(self.chat, Instant::now());
        proof.session_generations.insert(self.chat, self.generation);
        drop(proof);
        ctx.continue_with(SessionMsg::Rehydrate);
        Ok(())
    }

    async fn on_stop(&mut self, _ctx: &ActorContext<Self::Msg>) -> ActorResult {
        // Models a per-conversation teardown flush. The deterministic delay
        // also holds the router's Evicting window open so phase 7's raced
        // injection observably lands in the membership buffer instead of
        // slipping in after Removed.
        tokio::time::sleep(EVICT_FLUSH).await;
        Ok(())
    }

    fn drain_policy(&self) -> DrainPolicy {
        // A retiring session must drain, not discard: a message the router
        // forwarded before it processed our Evict would otherwise die with
        // this incarnation instead of being bounced back for the replacement.
        DrainPolicy::Drain
    }

    async fn handle(&mut self, message: Self::Msg, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        match message {
            SessionMsg::Rehydrate => {
                // Deliberate cut: a restarted incarnation cannot re-watch a
                // still-live run child, because watches die with the observer
                // and RuntimeHandle offers no typed-ref lookup by child id —
                // the ActorRef<RunMsg> is only minted by add_actor itself. In
                // this example the gap is unobservable (sessions never restart
                // while a run is live); a production owner would need to park
                // run refs outside the incarnation or a library-side lookup.
                let replay = self
                    .journal
                    .call(PHASE_TIMEOUT, |reply| JournalMsg::Replay {
                        chat: self.chat,
                        reply,
                    })
                    .await?;
                self.transcript_len = replay
                    .iter()
                    .filter(|entry| matches!(entry.entry, JournalEntry::UserMessage { .. }))
                    .count();
                self.proof
                    .lock()
                    .expect("proof lock poisoned")
                    .session_rehydrated_at
                    .insert(self.chat, Instant::now());
                self.arm_idle(ctx);
            }
            SessionMsg::UserMessage { envelope, text } => {
                if self.evict_requested {
                    // This incarnation has already asked the router to remove
                    // it; accepting new work here would lose it when removal
                    // lands. Bounce the message back to the router: our Evict
                    // was enqueued there before this bounce can be, so the
                    // router is guaranteed to buffer it for the replacement
                    // session rather than forward it back to us.
                    self.router
                        .send(RouterMsg::UserMessage {
                            envelope,
                            chat: self.chat,
                            text,
                        })
                        .await?;
                    return Ok(());
                }
                self.transcript_len += 1;
                self.idle_generation += 1;
                let input = PendingInput { envelope, text };
                if self.active.is_some() || !self.gate.load(Ordering::Acquire) {
                    self.pending.push_back(input);
                    if !self.gate.load(Ordering::Acquire) {
                        self.outbound
                            .send(OutboundMsg::Notice {
                                chat: self.chat,
                                text: "agent control is paused; task journaled".into(),
                            })
                            .await?;
                    }
                } else {
                    self.start_input(input, ctx).await?;
                }
            }
            SessionMsg::RunFinished { task, role, output } => {
                let Some(active) = self.active.as_mut() else {
                    return Ok(());
                };
                if active.task != task || active.role != role {
                    return Ok(());
                }
                let id = active.id.clone();
                match output {
                    RunOutput::Planned(plan) => {
                        tracing::debug!(chat = self.chat, task, %plan, "planner completed");
                        let input = active.input.clone();
                        self.pipeline_remove(id, ctx);
                        self.active = None;
                        self.start_run(task, Role::Engineer, 0, input, ctx).await?;
                    }
                    RunOutput::Engineered(output) => {
                        tracing::debug!(chat = self.chat, task, %output, "engineer completed");
                        let input = active.input.clone();
                        self.pipeline_remove(id, ctx);
                        self.active = None;
                        self.start_run(task, Role::Reviewer, 0, input, ctx).await?;
                    }
                    RunOutput::Reviewed(approved) => {
                        self.pipeline_remove(id, ctx);
                        self.complete_task(task, approved, ctx).await?;
                        if self.gate.load(Ordering::Acquire)
                            && let Some(input) = self.pending.pop_front()
                        {
                            self.start_input(input, ctx).await?;
                        }
                    }
                    RunOutput::RetryableFailure => {
                        if !active.failure_reported {
                            active.failure_reported = true;
                            self.guard
                                .send(GuardMsg::RunFailureObserved {
                                    chat: self.chat,
                                    task,
                                })
                                .await?;
                        }
                        active.retry_after_remove = true;
                        self.pipeline_remove(id, ctx);
                    }
                    RunOutput::Cancelled => {
                        self.pipeline_remove(id, ctx);
                        self.active = None;
                        if let Some(timer) = self.heartbeat.take() {
                            timer.cancel();
                        }
                        self.proof
                            .lock()
                            .expect("proof lock poisoned")
                            .run_terminal_at
                            .insert(self.chat, Instant::now());
                        self.arm_idle(ctx);
                    }
                }
            }
            SessionMsg::RunEvent { task, role, event } => {
                self.proof
                    .lock()
                    .expect("proof lock poisoned")
                    .monitor_events
                    .entry(task)
                    .or_default()
                    .push(event.clone());
                if let tokio_otp::MonitorEvent::Down(down) = &event
                    && down.reason == tokio_otp::DownReason::Failure
                {
                    let mut remove = None;
                    if let Some(active) = self.active.as_mut()
                        && active.task == task
                        && active.role == role
                    {
                        if !active.failure_reported {
                            active.failure_reported = true;
                            self.guard
                                .send(GuardMsg::RunFailureObserved {
                                    chat: self.chat,
                                    task,
                                })
                                .await?;
                        }
                        active.retry_after_remove = true;
                        remove = Some(active.id.clone());
                    }
                    if let Some(id) = remove {
                        self.pipeline_remove(id, ctx);
                    }
                }
            }
            SessionMsg::RunRemoved { id } => {
                if let Some(active) = self.active.take() {
                    if active.id != id {
                        self.active = Some(active);
                    } else if active.retry_after_remove && self.gate.load(Ordering::Acquire) {
                        self.start_run(
                            active.task,
                            active.role,
                            active.attempt + 1,
                            active.input,
                            ctx,
                        )
                        .await?;
                    } else if active.retry_after_remove {
                        self.pending.push_front(active.input);
                        if let Some(timer) = self.heartbeat.take() {
                            timer.cancel();
                        }
                    }
                }
            }
            SessionMsg::PauseChanged { paused } => {
                if paused {
                    self.outbound
                        .send(OutboundMsg::Notice {
                            chat: self.chat,
                            text: "agent control paused".into(),
                        })
                        .await?;
                    if let Some(active) = &self.active {
                        self.pending.push_front(active.input.clone());
                        active.cancel.cancel();
                    }
                } else if self.active.is_none()
                    && let Some(input) = self.pending.pop_front()
                {
                    self.start_input(input, ctx).await?;
                }
            }
            SessionMsg::Stop => {
                if let Some(active) = &self.active {
                    active.cancel.cancel();
                    self.append(JournalEntry::TaskCancelled { task: active.task })
                        .await?;
                }
            }
            SessionMsg::IdleSweep { generation } => {
                if generation == self.idle_generation && self.active.is_none() {
                    let task = self.task_sequence.load(Ordering::Relaxed);
                    self.append(JournalEntry::Checkpoint {
                        task,
                        state: format!("{} transcript item(s)", self.transcript_len),
                    })
                    .await?;
                    self.append(JournalEntry::Evicted).await?;
                    self.router
                        .send(RouterMsg::Evict {
                            chat: self.chat,
                            generation: self.generation,
                        })
                        .await?;
                    self.evict_requested = true;
                }
            }
        }
        Ok(())
    }
}
