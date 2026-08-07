mod read_only_shell;

pub use read_only_shell::{ShellCommandClass, classify_shell_command};

use agent_protocol::{
    ApprovalMode, AutoApproval, BudgetDimension, CheckpointSnapshot, EventEnvelope, ModelErrorKind,
    ModelFinishReason, ModelStreamEvent, RunStatus, SubagentResultDelivery, SubagentSpawnRequest,
    ToolApprovalPolicySnapshot, ToolApprovalRequest, ToolCall, ToolDescriptor, ToolEffect,
    ToolExecutionRequest,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunCommand {
    Start,
    RequireApproval,
    Approve,
    Complete,
    Cancel,
    ToolOutcomeUnknown { effect: ToolEffect },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TransitionError {
    #[error("run is already terminal: {0:?}")]
    TerminalState(RunStatus),
    #[error("command {command:?} is invalid while run is {status:?}")]
    InvalidTransition {
        status: RunStatus,
        command: RunCommand,
    },
    #[error("model stream event is invalid while run is {0:?}")]
    ModelEventOutsideRunning(RunStatus),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RegistryError {
    #[error("tool is already registered: {0}")]
    DuplicateTool(String),
    #[error("tool is not registered: {0}")]
    UnknownTool(String),
    #[error("tool implementation digest must be a lowercase SHA-256")]
    InvalidImplementationDigest,
    #[error("delegated scope is missing: {0:?}")]
    MissingScopes(BTreeSet<String>),
}

#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolDescriptor>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolPlan {
    Execute(ToolExecutionRequest),
    /// An approval-gated Tool whose policy exempted this particular call.
    ///
    /// Deliberately not `Execute`. An exempted call and a Tool that was never
    /// gated look the same to anything downstream if they share a variant, and
    /// then the ledger cannot say why no approval was asked for. Carrying the
    /// snapshot, its digest and a stated reason is what makes the exemption
    /// auditable rather than merely claimed.
    AutoApproved {
        execution: ToolExecutionRequest,
        policy_snapshot: ToolApprovalPolicySnapshot,
        policy_digest: String,
        reason: String,
    },
    ApprovalRequired(ToolApprovalRequest),
    Denied(ToolExecutionRequest),
    SubagentSpawn(agent_protocol::SubagentSpawnRequest),
}

impl ToolRegistry {
    pub fn register(&mut self, tool: ToolDescriptor) -> Result<(), RegistryError> {
        if tool.implementation_digest.len() != 64
            || !tool
                .implementation_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RegistryError::InvalidImplementationDigest);
        }
        if self.tools.contains_key(&tool.name) {
            return Err(RegistryError::DuplicateTool(tool.name));
        }
        self.tools.insert(tool.name.clone(), tool);
        Ok(())
    }

    pub fn authorize(
        &self,
        name: &str,
        delegated_scopes: &BTreeSet<String>,
    ) -> Result<&ToolDescriptor, RegistryError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| RegistryError::UnknownTool(name.to_owned()))?;
        let missing = tool
            .required_scopes
            .difference(delegated_scopes)
            .cloned()
            .collect::<BTreeSet<_>>();
        if missing.is_empty() {
            Ok(tool)
        } else {
            Err(RegistryError::MissingScopes(missing))
        }
    }

    pub fn plan(
        &self,
        call: ToolCall,
        delegated_scopes: &BTreeSet<String>,
        // The Run's policy, from the execution command. Absent means ask, so a
        // command that says nothing about a Tool cannot be read as granting it
        // anything.
        tool_approval_policies: &BTreeMap<String, AutoApproval>,
    ) -> Result<ToolPlan, RegistryError> {
        let descriptor = self.authorize(&call.name, delegated_scopes)?;
        let auto_approval = tool_approval_policies
            .get(&descriptor.name)
            .copied()
            .unwrap_or_default();
        let policy_snapshot = ToolApprovalPolicySnapshot {
            tool_name: descriptor.name.clone(),
            effect: descriptor.effect,
            approval: descriptor.approval,
            sandbox: descriptor.sandbox,
            implementation_digest: descriptor.implementation_digest.clone(),
            required_scopes: descriptor.required_scopes.clone(),
            auto_approval,
        };
        let policy_digest = digest(&policy_snapshot);
        let session_scope_digest = digest(&json!({
            "arguments": &call.arguments,
            "policy_snapshot": &policy_snapshot,
            "tool_name": &call.name,
        }));
        // The approval policy is part of the binding. Without it a call that was
        // gated and the same call that was exempted produced identical digests,
        // so a decision taken under one policy bound just as well to an
        // execution under another.
        let binding_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&(
                &call,
                descriptor.effect,
                descriptor.sandbox,
                &descriptor.implementation_digest,
                &descriptor.required_scopes,
                descriptor.approval,
                auto_approval,
            ))
            .expect("tool authorization binding must be serializable"),
        ));
        let execution = ToolExecutionRequest {
            call,
            effect: descriptor.effect,
            sandbox: descriptor.sandbox,
            binding_digest,
        };
        // An `Ask` Tool may exempt a particular call, but only through a policy
        // its descriptor declares. The Worker never decides this on its own:
        // the exemption travels with the Tool definition and is recorded in the
        // policy snapshot below, so it stays auditable.
        let exempt = descriptor.approval == ApprovalMode::Ask
            && auto_approval == AutoApproval::ProvablyReadOnlyShellCommand
            && execution
                .call
                .arguments
                .get("command")
                .and_then(|command| command.as_str())
                .map(classify_shell_command)
                == Some(ShellCommandClass::ProvablyReadOnly);
        if exempt {
            return Ok(ToolPlan::AutoApproved {
                execution,
                policy_snapshot,
                policy_digest,
                reason: "shell command classified provably read-only".to_owned(),
            });
        }
        Ok(match descriptor.approval {
            ApprovalMode::Allow => ToolPlan::Execute(execution),
            ApprovalMode::Ask => ToolPlan::ApprovalRequired(ToolApprovalRequest {
                approval_id: Uuid::now_v7(),
                execution,
                policy_snapshot: Some(policy_snapshot),
                policy_digest,
                session_scope_digest,
            }),
            ApprovalMode::Deny => ToolPlan::Denied(execution),
        })
    }
}

fn digest(value: &impl serde::Serialize) -> String {
    let value = serde_json::to_value(value).expect("tool approval scope must be serializable");
    hex::encode(Sha256::digest(
        serde_json::to_vec(&canonicalize(value))
            .expect("canonical tool approval scope must be serializable"),
    ))
}

fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize).collect())
        }
        scalar => scalar,
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("checkpoint digest is invalid")]
pub struct InvalidCheckpoint;

#[derive(Clone, Debug)]
pub struct RunMachine {
    run_id: Uuid,
    tenant_id: Uuid,
    session_id: Uuid,
    attempt_id: Uuid,
    status: RunStatus,
    sequence: u64,
}

impl RunMachine {
    #[must_use]
    pub const fn new(run_id: Uuid, tenant_id: Uuid, session_id: Uuid, attempt_id: Uuid) -> Self {
        Self {
            run_id,
            tenant_id,
            session_id,
            attempt_id,
            status: RunStatus::Queued,
            sequence: 0,
        }
    }

    #[must_use]
    pub const fn status(&self) -> RunStatus {
        self.status
    }

    #[must_use]
    pub fn checkpoint(&self, state: Vec<u8>) -> CheckpointSnapshot {
        CheckpointSnapshot::new(
            self.run_id,
            self.tenant_id,
            self.session_id,
            self.attempt_id,
            self.status,
            self.sequence,
            state,
        )
    }

    pub fn from_checkpoint(checkpoint: CheckpointSnapshot) -> Result<Self, InvalidCheckpoint> {
        if !checkpoint.verify_digest() {
            return Err(InvalidCheckpoint);
        }
        Ok(Self {
            run_id: checkpoint.run_id,
            tenant_id: checkpoint.tenant_id,
            session_id: checkpoint.session_id,
            attempt_id: checkpoint.attempt_id,
            status: checkpoint.status,
            sequence: checkpoint.sequence,
        })
    }

    pub fn from_checkpoint_for_attempt(
        checkpoint: CheckpointSnapshot,
        attempt_id: Uuid,
    ) -> Result<Self, InvalidCheckpoint> {
        if attempt_id.is_nil() || !checkpoint.verify_digest() {
            return Err(InvalidCheckpoint);
        }
        Ok(Self {
            run_id: checkpoint.run_id,
            tenant_id: checkpoint.tenant_id,
            session_id: checkpoint.session_id,
            attempt_id,
            status: checkpoint.status,
            sequence: checkpoint.sequence,
        })
    }

    pub fn record_restored(
        &mut self,
        checkpoint_digest: &str,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        Ok(self.emit(
            self.status,
            "run.restored",
            json!({
                "status": self.status,
                "checkpoint_digest": checkpoint_digest
            }),
        ))
    }

    pub fn record_steering_applied(
        &mut self,
        steering_id: Uuid,
        input_digest: &str,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Start,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "run.steer.applied",
            json!({
                "status": RunStatus::Running,
                "steering_id": steering_id,
                "input_digest": input_digest
            }),
        ))
    }

    pub fn apply(&mut self, command: RunCommand) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }

        let (next_status, event_type) = match (self.status, command) {
            (RunStatus::Queued, RunCommand::Start) => (RunStatus::Running, "run.started"),
            (RunStatus::Running, RunCommand::RequireApproval) => {
                (RunStatus::WaitingApproval, "approval.required")
            }
            (RunStatus::WaitingApproval, RunCommand::Approve) => {
                (RunStatus::Running, "run.resumed")
            }
            (RunStatus::Running, RunCommand::Complete) => (RunStatus::Succeeded, "run.succeeded"),
            (
                RunStatus::Running,
                RunCommand::ToolOutcomeUnknown {
                    effect: ToolEffect::NonIdempotent | ToolEffect::Unknown,
                },
            ) => (RunStatus::Indeterminate, "run.indeterminate"),
            (
                RunStatus::Running,
                RunCommand::ToolOutcomeUnknown {
                    effect: ToolEffect::Pure | ToolEffect::Idempotent,
                },
            ) => (RunStatus::Running, "tool.retry_requested"),
            (
                RunStatus::Queued
                | RunStatus::Running
                | RunStatus::WaitingApproval
                | RunStatus::Suspended,
                RunCommand::Cancel,
            ) => (RunStatus::Cancelled, "run.cancelled"),
            (status, command) => {
                return Err(TransitionError::InvalidTransition { status, command });
            }
        };

        Ok(self.emit(next_status, event_type, json!({ "status": next_status })))
    }

    pub fn apply_model_event(
        &mut self,
        event: ModelStreamEvent,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::ModelEventOutsideRunning(self.status));
        }

        let envelope = match event {
            ModelStreamEvent::TextDelta { text } => self.emit(
                RunStatus::Running,
                "model.output.delta",
                json!({ "text": text }),
            ),
            ModelStreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => self.emit(
                RunStatus::Running,
                "model.tool_call",
                json!({ "id": id, "name": name, "arguments": arguments }),
            ),
            ModelStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cost_micros,
            } => self.emit(
                RunStatus::Running,
                "model.usage",
                json!({
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cost_micros": cost_micros
                }),
            ),
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            } => self.emit(
                RunStatus::Succeeded,
                "run.succeeded",
                json!({ "status": RunStatus::Succeeded, "reason": ModelFinishReason::Stop }),
            ),
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            } => self.emit(
                RunStatus::Running,
                "model.turn.completed",
                json!({ "reason": ModelFinishReason::ToolCalls }),
            ),
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Length,
            } => self.emit(
                RunStatus::Failed,
                "run.failed",
                json!({
                    "status": RunStatus::Failed,
                    "kind": ModelErrorKind::ContextOverflow,
                    "reason": ModelFinishReason::Length
                }),
            ),
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ContentFilter,
            } => self.emit(
                RunStatus::Failed,
                "run.failed",
                json!({
                    "status": RunStatus::Failed,
                    "kind": ModelErrorKind::Protocol,
                    "reason": ModelFinishReason::ContentFilter
                }),
            ),
            ModelStreamEvent::Failed {
                kind,
                retryable,
                message,
            } => {
                let (status, event_type) = if kind == ModelErrorKind::Timeout {
                    (RunStatus::TimedOut, "run.timed_out")
                } else {
                    (RunStatus::Failed, "run.failed")
                };
                self.emit(
                    status,
                    event_type,
                    json!({
                        "status": status,
                        "kind": kind,
                        "retryable": retryable,
                        "message": message
                    }),
                )
            }
        };
        Ok(envelope)
    }

    pub fn record_budget_exhausted(
        &mut self,
        dimension: BudgetDimension,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::ModelEventOutsideRunning(self.status));
        }
        Ok(self.emit(
            RunStatus::Failed,
            "run.failed",
            json!({
                "status": RunStatus::Failed,
                "kind": "budget_exhausted",
                "dimension": dimension,
                "retryable": false
            }),
        ))
    }

    pub fn record_subagent_spawn_requested(
        &mut self,
        request: &SubagentSpawnRequest,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::RequireApproval,
            });
        }
        Ok(self.emit(
            RunStatus::Suspended,
            "subagent.spawn.requested",
            json!({"status": RunStatus::Suspended, "request": request}),
        ))
    }

    pub fn record_subagent_result_received(
        &mut self,
        result: &SubagentResultDelivery,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Suspended {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Approve,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "subagent.result.received",
            json!({
                "status": RunStatus::Running,
                "tool_call_id": result.tool_call_id,
                "delegation_id": result.delegation_id,
                "binding_digest": result.binding_digest,
                "child_run_id": result.child_run_id,
                "child_terminal_event_id": result.child_terminal_event_id,
                "terminal_status": result.terminal_status,
                "is_error": result.is_error,
                "result_digest": result.digest
            }),
        ))
    }

    pub fn apply_tool_plan(&mut self, plan: &ToolPlan) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::RequireApproval,
            });
        }
        let (status, event_type, payload) = match plan {
            ToolPlan::Execute(execution) => (
                RunStatus::Running,
                "tool.execution.requested",
                json!({"execution": execution}),
            ),
            // Its own event type, not `tool.execution.requested`. An exempted
            // call that emitted the ordinary event would be indistinguishable in
            // the durable log from a Tool that was never approval gated, and
            // "no approval was asked for" would have no recorded reason. The
            // snapshot, its digest and the reason are all persisted here.
            ToolPlan::AutoApproved {
                execution,
                policy_snapshot,
                policy_digest,
                reason,
            } => (
                RunStatus::Running,
                "tool.execution.auto_approved",
                json!({
                    "execution": execution,
                    "policy_snapshot": policy_snapshot,
                    "policy_digest": policy_digest,
                    "reason": reason,
                }),
            ),
            ToolPlan::ApprovalRequired(approval) => (
                RunStatus::WaitingApproval,
                "approval.required",
                json!({"approval": approval, "status": RunStatus::WaitingApproval}),
            ),
            ToolPlan::Denied(execution) => (
                RunStatus::Running,
                "tool.denied",
                json!({"execution": execution, "reason": "policy_denied"}),
            ),
            ToolPlan::SubagentSpawn(request) => {
                return self.record_subagent_spawn_requested(request);
            }
        };
        Ok(self.emit(status, event_type, payload))
    }

    pub fn record_tool_result(
        &mut self,
        tool_call_id: &str,
        binding_digest: &str,
        content: serde_json::Value,
        is_error: bool,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "tool.result",
            json!({
                "tool_call_id": tool_call_id,
                "binding_digest": binding_digest,
                "content": content,
                "is_error": is_error
            }),
        ))
    }

    pub fn record_tool_execution_started(
        &mut self,
        execution: &ToolExecutionRequest,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "tool.execution.started",
            json!({"execution": execution}),
        ))
    }

    pub fn record_approval_rebound(
        &mut self,
        approval: &ToolApprovalRequest,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::WaitingApproval {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::RequireApproval,
            });
        }
        Ok(self.emit(
            RunStatus::WaitingApproval,
            "approval.rebound",
            json!({"approval": approval, "status": RunStatus::WaitingApproval}),
        ))
    }

    fn emit(
        &mut self,
        next_status: RunStatus,
        event_type: &str,
        payload: serde_json::Value,
    ) -> EventEnvelope {
        self.status = next_status;
        self.sequence += 1;
        EventEnvelope::new(
            self.tenant_id,
            self.session_id,
            self.run_id,
            self.sequence,
            self.attempt_id,
            event_type,
            payload,
        )
    }
}
