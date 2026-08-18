mod read_only_shell;

pub use read_only_shell::{ShellCommandClass, classify_shell_command};

use agent_protocol::{
    ApprovalMode, AutoApproval, BudgetDimension, CheckpointSnapshot, EventEnvelope,
    MCP_INPUT_VERSION, McpInputContinuation, McpInputRequired, McpServerDiscoveryStatus,
    ModelErrorKind, ModelFinishReason, ModelStreamEvent, RunStatus, SandboxClass,
    SubagentForkReceipt, SubagentResultDelivery, SubagentRollbackReceipt, SubagentSpawnMode,
    SubagentSpawnRequest, ToolApprovalPolicySnapshot, ToolApprovalRequest, ToolCall,
    ToolDescriptor, ToolEffect, ToolExecutionRequest,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use uuid::Uuid;

pub struct SubagentInputAcceptance<'a> {
    pub agent_id: Uuid,
    pub message_sequence: u64,
    pub idempotency_key: &'a str,
    pub submission_id: &'a str,
    pub status: &'a str,
    pub interrupt: bool,
    pub request: &'a SubagentSpawnRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunCommand {
    Start,
    RequireApproval,
    Approve,
    Complete,
    Cancel,
    RequestInput,
    ResumeInput,
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
            // No exemption reaches a federated Tool (ADR-0040 decision 6). The
            // read-only exemption rests on knowing the command cannot write, and
            // nothing is known about third-party code by construction. Without
            // this, a federated tool taking an argument called `command` -- an
            // entirely ordinary thing for a tool to take -- would be exempted by
            // a classifier written for a shell it is not.
            //
            // Checked on the descriptor's sandbox class rather than on a name
            // prefix: a name is a string a registration chooses, and the class
            // is what the platform decided about how the Tool is confined.
            && descriptor.sandbox != SandboxClass::Federated
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

    pub fn record_context_compacted(
        &mut self,
        binding_digest: &str,
        source_transcript_digest: &str,
        summary_digest: &str,
        retained_tail_digest: &str,
        source_message_count: u32,
        retained_message_count: u32,
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
            "context.compacted",
            json!({
                "status": RunStatus::Running,
                "binding_digest": binding_digest,
                "source_transcript_digest": source_transcript_digest,
                "summary_digest": summary_digest,
                "retained_tail_digest": retained_tail_digest,
                "source_message_count": source_message_count,
                "retained_message_count": retained_message_count,
            }),
        ))
    }

    pub fn apply(&mut self, command: RunCommand) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        let unknown_tool_effect = match &command {
            RunCommand::ToolOutcomeUnknown { effect } => Some(*effect),
            _ => None,
        };

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

        let mut payload = json!({ "status": next_status });
        if event_type == "run.indeterminate" {
            payload["effect"] = json!(unknown_tool_effect.expect("indeterminate has Tool effect"));
            payload["replay_safe"] = json!(false);
        }
        Ok(self.emit(next_status, event_type, payload))
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
            ModelStreamEvent::TextDelta { text, block } => self.emit(
                RunStatus::Running,
                "model.output.delta",
                // The block travels into the durable log, because that is where
                // a client reads a Run it did not watch happen. Omitted when the
                // provider gave none, so a record written before this existed
                // and a stream that genuinely has no blocks read the same.
                match block {
                    Some(block) => json!({ "text": text, "block": block }),
                    None => json!({ "text": text }),
                },
            ),
            // Beside `model.output.delta` and shaped the same, because it is
            // the same kind of thing: something the model is producing, now.
            // A reasoning model spends most of a Turn here, and a client that
            // cannot see it is a client showing a frozen screen.
            ModelStreamEvent::ReasoningDelta { text, block } => self.emit(
                RunStatus::Running,
                "model.reasoning.delta",
                match block {
                    Some(block) => json!({ "text": text, "block": block }),
                    None => json!({ "text": text }),
                },
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
            ModelStreamEvent::Reasoning {
                summary,
                private_state,
            } => self.emit(
                RunStatus::Running,
                "model.reasoning",
                json!({
                    "summary": summary,
                    "has_private_state": private_state.is_some()
                }),
            ),
            ModelStreamEvent::Refusal { text } => {
                self.emit(RunStatus::Running, "model.refusal", json!({ "text": text }))
            }
            ModelStreamEvent::PrivateStateOmitted {
                origin_provider_id,
                target_provider_id,
                format,
            } => self.emit(
                RunStatus::Running,
                "model.private_state.omitted",
                json!({
                    "origin_provider_id": origin_provider_id,
                    "target_provider_id": target_provider_id,
                    "format": format
                }),
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

    pub fn record_model_provider_failure(
        &mut self,
        provider_id: &str,
        kind: ModelErrorKind,
        retryable: bool,
        status: Option<u16>,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status != RunStatus::Running {
            return Err(TransitionError::ModelEventOutsideRunning(self.status));
        }
        Ok(self.emit(
            RunStatus::Running,
            "model.provider.failed",
            json!({
                "provider_id": provider_id,
                "kind": kind,
                "retryable": retryable,
                "status": status,
            }),
        ))
    }

    pub fn record_model_provider_retry_scheduled(
        &mut self,
        provider_id: &str,
        provider_attempt: u8,
        delay_ms: u64,
        kind: ModelErrorKind,
        status: Option<u16>,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status != RunStatus::Running {
            return Err(TransitionError::ModelEventOutsideRunning(self.status));
        }
        Ok(self.emit(
            RunStatus::Running,
            "model.provider.retry_scheduled",
            json!({
                "provider_id": provider_id,
                "provider_attempt": provider_attempt,
                "delay_ms": delay_ms,
                "kind": kind,
                "status": status,
            }),
        ))
    }

    pub fn record_model_provider_selection(
        &mut self,
        provider_id: &str,
        failed_provider_ids: &[String],
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status != RunStatus::Running {
            return Err(TransitionError::ModelEventOutsideRunning(self.status));
        }
        Ok(self.emit(
            RunStatus::Running,
            "model.provider.selected",
            json!({
                "provider_id": provider_id,
                "failed_provider_ids": failed_provider_ids,
            }),
        ))
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

    pub fn record_required_mcp_unavailable(
        &mut self,
        server_names: &[String],
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if !matches!(self.status, RunStatus::Queued | RunStatus::Running) {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Start,
            });
        }
        Ok(self.emit(
            RunStatus::Failed,
            "run.failed",
            json!({
                "status": RunStatus::Failed,
                "kind": "required_mcp_unavailable",
                "servers": server_names,
                "retryable": false
            }),
        ))
    }

    pub fn record_duration_timed_out(&mut self) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        Ok(self.emit(
            RunStatus::TimedOut,
            "run.timed_out",
            json!({
                "status": RunStatus::TimedOut,
                "kind": "duration_budget_exhausted",
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
        if !matches!(self.status, RunStatus::Running | RunStatus::Suspended) {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::RequireApproval,
            });
        }
        let status = match request.mode {
            SubagentSpawnMode::Inline => RunStatus::Suspended,
            SubagentSpawnMode::Async => RunStatus::Running,
        };
        Ok(self.emit(
            status,
            "subagent.spawn.requested",
            json!({"status": status, "request": request}),
        ))
    }

    pub fn record_subagent_spawned(
        &mut self,
        request: &SubagentSpawnRequest,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running || request.mode != SubagentSpawnMode::Async {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "subagent.spawned",
            json!({
                "agent_id": request.delegation_id,
                "role": request.role,
                "status": "running"
            }),
        ))
    }

    pub fn record_subagent_forked(
        &mut self,
        receipt: &SubagentForkReceipt,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running || !receipt.is_well_formed() {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "subagent.forked",
            json!({
                "source_agent_id": receipt.source_agent_id,
                "source_generation": receipt.source_generation,
                "through_activation_ordinal": receipt.through_activation_ordinal,
                "source_history_digest": receipt.source_history_digest,
                "agent_id": receipt.agent_id,
                "generation": receipt.generation,
                "role": receipt.role,
                "budget": receipt.budget
            }),
        ))
    }

    pub fn record_subagent_rolled_back(
        &mut self,
        receipt: &SubagentRollbackReceipt,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running || !receipt.is_well_formed() {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "subagent.rolled_back",
            json!({
                "agent_id": receipt.agent_id,
                "from_generation": receipt.from_generation,
                "generation": receipt.generation,
                "through_activation_ordinal": receipt.through_activation_ordinal,
                "previous_history_digest": receipt.previous_history_digest,
                "restored_history_digest": receipt.restored_history_digest
            }),
        ))
    }

    pub fn record_subagent_terminal_observed(
        &mut self,
        result: &SubagentResultDelivery,
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
            "subagent.terminal.observed",
            json!({
                "agent_id": result.delegation_id,
                "child_run_id": result.child_run_id,
                "child_terminal_event_id": result.child_terminal_event_id,
                "terminal_status": result.terminal_status,
                "is_error": result.is_error,
                "usage": result.usage,
                "result_digest": result.digest
            }),
        ))
    }

    pub fn record_subagent_input_accepted(
        &mut self,
        acceptance: SubagentInputAcceptance<'_>,
    ) -> Result<EventEnvelope, TransitionError> {
        let SubagentInputAcceptance {
            agent_id,
            message_sequence,
            idempotency_key,
            submission_id,
            status,
            interrupt,
            request,
        } = acceptance;
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running || request.mode != SubagentSpawnMode::Async {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "subagent.input.accepted",
            json!({
                "agent_id": agent_id,
                "message_sequence": message_sequence,
                "idempotency_key": idempotency_key,
                "submission_id": submission_id,
                "child_run_id": request.delegation_id,
                "status": status,
                "interrupt": interrupt
            }),
        ))
    }

    pub fn record_subagent_input_activated(
        &mut self,
        agent_id: Uuid,
        message_sequence: u64,
        request: &SubagentSpawnRequest,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running || request.mode != SubagentSpawnMode::Async {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::Complete,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "subagent.input.activated",
            json!({
                "agent_id": agent_id,
                "message_sequence": message_sequence,
                "child_run_id": request.delegation_id,
                "status": "running"
            }),
        ))
    }

    pub fn record_subagent_closed(
        &mut self,
        agent_id: Uuid,
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
            "subagent.closed",
            json!({
                "agent_id": agent_id,
                "status": "closed"
            }),
        ))
    }

    pub fn record_subagent_result_received(
        &mut self,
        result: &SubagentResultDelivery,
    ) -> Result<EventEnvelope, TransitionError> {
        self.record_subagent_result_received_with_remaining(result, 0)
    }

    pub fn record_subagent_result_received_with_remaining(
        &mut self,
        result: &SubagentResultDelivery,
        remaining_subagents: usize,
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
        let next_status = if remaining_subagents == 0 {
            RunStatus::Running
        } else {
            RunStatus::Suspended
        };
        Ok(self.emit(
            next_status,
            "subagent.result.received",
            json!({
                "status": next_status,
                "remaining_subagents": remaining_subagents,
                "tool_call_id": result.tool_call_id,
                "delegation_id": result.delegation_id,
                "binding_digest": result.binding_digest,
                "child_run_id": result.child_run_id,
                "child_terminal_event_id": result.child_terminal_event_id,
                "terminal_status": result.terminal_status,
                "is_error": result.is_error,
                "usage": result.usage,
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

    pub fn record_tool_execution_progress(
        &mut self,
        tool_call_id: &str,
        binding_digest: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
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
            "tool.execution.progress",
            json!({
                "tool_call_id": tool_call_id,
                "binding_digest": binding_digest,
                "progress": progress,
                "total": total,
                "message": message,
            }),
        ))
    }

    pub fn record_mcp_input_required(
        &mut self,
        pending: &McpInputRequired,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::RequestInput,
            });
        }
        Ok(self.emit(
            RunStatus::Suspended,
            "mcp.input.required",
            json!({
                "input": pending,
                "input_version": MCP_INPUT_VERSION,
                "status": RunStatus::Suspended,
            }),
        ))
    }

    pub fn record_mcp_input_resolved(
        &mut self,
        pending: &McpInputRequired,
        continuation: &McpInputContinuation,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Suspended {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::ResumeInput,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "mcp.input.resolved",
            json!({
                "input_id": pending.input_id,
                "tool_call_id": pending.tool_call_id,
                "binding_digest": pending.binding_digest,
                "round": continuation.round,
                "actions": continuation.responses.iter().map(|(key, response)| {
                    (key, response.action)
                }).collect::<BTreeMap<_, _>>(),
                "status": RunStatus::Running
            }),
        ))
    }

    pub fn record_mcp_continuation_started(
        &mut self,
        pending: &McpInputRequired,
        continuation: &McpInputContinuation,
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        if self.status != RunStatus::Running {
            return Err(TransitionError::InvalidTransition {
                status: self.status,
                command: RunCommand::ResumeInput,
            });
        }
        Ok(self.emit(
            RunStatus::Running,
            "mcp.input.continuation.started",
            json!({
                "input_id": pending.input_id,
                "tool_call_id": pending.tool_call_id,
                "binding_digest": pending.binding_digest,
                "round": continuation.round
            }),
        ))
    }

    /// Every configured MCP server's discovery outcome, once, at the point the
    /// Run became able to use them.
    ///
    /// The terminal `run.failed{required_mcp_unavailable}` already names a
    /// *required* server that did not come up. An **optional** one that did not
    /// come up leaves no trace at all: the Run carries on without those Tools,
    /// and nothing downstream can tell "the model did not use it" from "it was
    /// never there". A person who configured a server and sees the agent ignore
    /// it has no way to find out which of those happened. This event is that
    /// trace, and it carries the healthy servers too so that the absence of one
    /// is readable rather than inferred from silence.
    pub fn record_mcp_discovery_completed(
        &mut self,
        servers: &[McpServerDiscoveryStatus],
    ) -> Result<EventEnvelope, TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::TerminalState(self.status));
        }
        // Says something without changing anything, so it carries the status
        // forward rather than naming one. The other shape was written first and
        // it resurrected a Run: discovery runs again when a replacement host
        // restores an attempt, and an attempt parked on `mcp.input.required`
        // restores as `Suspended` -- which an event that names `Running` would
        // quietly undo, on exactly the path where a server is most likely to
        // have gone away.
        let status = self.status;
        Ok(self.emit(
            status,
            "mcp.discovery.completed",
            json!({ "servers": servers, "status": status }),
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
