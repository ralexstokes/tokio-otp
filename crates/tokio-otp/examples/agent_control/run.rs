//! One transient role run, implemented as a mailbox-driven state machine.

use std::sync::Arc;

use tokio_otp::{Actor, ActorContext, ActorRef, ActorResult, CancellationToken};

use crate::{
    messages::{
        BudgetMsg, ChatId, EffectStatus, JournalEntry, JournalMsg, MODEL_DEADLINE, ModelAction,
        ModelError, PHASE_TIMEOUT, ProgressMsg, Role, RunMsg, RunOutput, SessionMsg, TOOL_DEADLINE,
        TaskId, ToolCall, ToolHostMsg, ToolOutcome, TurnRequest,
    },
    model::ModelClient,
};

pub struct AgentRun {
    chat: ChatId,
    task: TaskId,
    role: Role,
    attempt: u64,
    user_text: String,
    model: Arc<dyn ModelClient>,
    journal: ActorRef<JournalMsg>,
    budget: ActorRef<BudgetMsg>,
    tool_host: ActorRef<ToolHostMsg>,
    progress: ActorRef<ProgressMsg>,
    session: ActorRef<SessionMsg>,
    cancel: CancellationToken,
    turn: u64,
    tools: Vec<ToolCall>,
}

#[derive(Clone)]
pub struct AgentRunFactory {
    pub chat: ChatId,
    pub task: TaskId,
    pub role: Role,
    pub attempt: u64,
    pub user_text: String,
    pub model: Arc<dyn ModelClient>,
    pub journal: ActorRef<JournalMsg>,
    pub budget: ActorRef<BudgetMsg>,
    pub tool_host: ActorRef<ToolHostMsg>,
    pub progress: ActorRef<ProgressMsg>,
    pub session: ActorRef<SessionMsg>,
    pub cancel: CancellationToken,
}

impl tokio_otp::ActorFactory for AgentRunFactory {
    type Actor = AgentRun;

    fn build(&self) -> Self::Actor {
        AgentRun {
            chat: self.chat,
            task: self.task,
            role: self.role,
            attempt: self.attempt,
            user_text: self.user_text.clone(),
            model: self.model.clone(),
            journal: self.journal.clone(),
            budget: self.budget.clone(),
            tool_host: self.tool_host.clone(),
            progress: self.progress.clone(),
            session: self.session.clone(),
            cancel: self.cancel.clone(),
            turn: 0,
            tools: Vec::new(),
        }
    }
}

impl AgentRun {
    async fn append(&self, entry: JournalEntry) -> ActorResult {
        tokio::time::timeout(
            PHASE_TIMEOUT,
            self.journal.call(|reply| JournalMsg::Append {
                chat: self.chat,
                entry,
                reply,
            }),
        )
        .await??;
        Ok(())
    }

    fn start_model(&self, ctx: &ActorContext<RunMsg>) {
        let request = TurnRequest {
            chat: self.chat,
            task: self.task,
            role: self.role,
            attempt: self.attempt,
            turn: self.turn,
            user_text: self.user_text.clone(),
            progress: self.progress.clone(),
        };
        let model = self.model.clone();
        let cancel = self.cancel.clone();
        let myself = ctx.myself();
        let turn = self.turn;
        tokio::spawn(async move {
            let result = tokio::time::timeout(MODEL_DEADLINE, model.turn(request, cancel))
                .await
                .unwrap_or(Err(ModelError::Deadline));
            let _ = myself.send(RunMsg::ModelResult { turn, result }).await;
        });
    }

    async fn start_tool(&self, index: usize, ctx: &ActorContext<RunMsg>) -> ActorResult {
        let call = self.tools[index].clone();
        let key = format!("{}:{}:{index}", self.chat, self.task);
        self.append(JournalEntry::ToolIntent {
            task: self.task,
            key: key.clone(),
            call: call.name.clone(),
        })
        .await?;
        let tool_host = self.tool_host.clone();
        let myself = ctx.myself();
        tokio::spawn(async move {
            let execute = tokio::time::timeout(
                TOOL_DEADLINE,
                tool_host.call(|reply| ToolHostMsg::Execute {
                    key: key.clone(),
                    call,
                    reply,
                }),
            )
            .await;
            let outcome = match execute {
                Ok(Ok(outcome)) => outcome,
                _ => {
                    // A timeout is an unknown outcome. Querying is ordered
                    // behind the in-flight Execute in the tool-host mailbox,
                    // so this deterministically reconciles the completed key.
                    match tokio::time::timeout(
                        PHASE_TIMEOUT,
                        tool_host.call(|reply| ToolHostMsg::Query {
                            key: key.clone(),
                            reply,
                        }),
                    )
                    .await
                    {
                        Ok(Ok(EffectStatus::Found(outcome))) => outcome,
                        _ => ToolOutcome {
                            output: "tool outcome remained unknown".into(),
                        },
                    }
                }
            };
            let _ = myself
                .send(RunMsg::ToolResult {
                    index,
                    key,
                    result: outcome,
                })
                .await;
        });
        Ok(())
    }

    async fn finish(&self, output: RunOutput) -> ActorResult {
        self.session
            .send(SessionMsg::RunFinished {
                task: self.task,
                role: self.role,
                output,
            })
            .await?;
        Ok(())
    }
}

impl Actor for AgentRun {
    type Msg = RunMsg;

    async fn on_start(&mut self, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        ctx.continue_with(RunMsg::Step);
        Ok(())
    }

    async fn handle(&mut self, message: Self::Msg, ctx: &ActorContext<Self::Msg>) -> ActorResult {
        match message {
            RunMsg::Step => self.start_model(ctx),
            RunMsg::ModelResult { turn, result } => {
                if turn != self.turn {
                    return Ok(());
                }
                let turn = match result {
                    Ok(turn) => turn,
                    Err(ModelError::Cancelled) => {
                        self.append(JournalEntry::Checkpoint {
                            task: self.task,
                            state: "model step cancelled".into(),
                        })
                        .await?;
                        self.finish(RunOutput::Cancelled).await?;
                        return Ok(());
                    }
                    Err(ModelError::RateLimited | ModelError::Deadline) => {
                        self.append(JournalEntry::Checkpoint {
                            task: self.task,
                            state: "retryable model failure".into(),
                        })
                        .await?;
                        self.finish(RunOutput::RetryableFailure).await?;
                        return Err(std::io::Error::other("model provider unavailable").into());
                    }
                };
                self.budget
                    .send(BudgetMsg::Charge {
                        chat: self.chat,
                        tokens: turn.tokens_spent,
                    })
                    .await?;
                self.progress
                    .send(ProgressMsg::Delta {
                        chat: self.chat,
                        line: format!("{:?} turn {} complete", self.role, self.turn),
                    })
                    .await?;
                match turn.action {
                    ModelAction::Plan(plan) => {
                        self.append(JournalEntry::Plan {
                            task: self.task,
                            text: plan.clone(),
                        })
                        .await?;
                        self.finish(RunOutput::Planned(plan)).await?;
                    }
                    ModelAction::Tools(tools) => {
                        self.tools = tools;
                        self.start_tool(0, ctx).await?;
                    }
                    ModelAction::Complete(output) => {
                        self.finish(RunOutput::Engineered(output)).await?;
                    }
                    ModelAction::Review(approved) => {
                        self.append(JournalEntry::Review {
                            task: self.task,
                            approved,
                        })
                        .await?;
                        self.finish(RunOutput::Reviewed(approved)).await?;
                    }
                }
            }
            RunMsg::ToolResult { index, key, result } => {
                self.append(JournalEntry::ToolEffect {
                    task: self.task,
                    key,
                    outcome: result,
                })
                .await?;
                if self.user_text.contains("PANIC-MIDRUN")
                    && self.role == Role::Engineer
                    && self.attempt == 0
                    && index == 0
                {
                    panic!("scripted engineer panic after tool effect");
                }
                let next = index + 1;
                if next < self.tools.len() {
                    self.start_tool(next, ctx).await?;
                } else {
                    self.turn += 1;
                    ctx.continue_with(RunMsg::Step);
                }
            }
        }
        Ok(())
    }
}
