use crate::{
    GrpcMcpFederationClient, McpDiscoverySupervisor, McpDiscoveryUpdate, McpServerDiscoveryStatus,
    McpServerHealth, WorkerAssignmentError, WorkerProcessor, attach_discovered_federated_tools,
};
use agent_protocol::{EventEnvelope, RunExecutionCommand};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum McpDiscoveryPurpose {
    Start,
    Restore,
}

#[derive(Clone, Debug)]
pub enum McpDiscoveryCompletion {
    Started {
        attempt_id: Uuid,
        event: EventEnvelope,
        mcp_servers: Vec<McpServerDiscoveryStatus>,
    },
    Restored {
        attempt_id: Uuid,
        mcp_servers: Vec<McpServerDiscoveryStatus>,
    },
    Failed {
        attempt_id: Uuid,
        event: EventEnvelope,
        mcp_servers: Vec<McpServerDiscoveryStatus>,
    },
    Cancelled {
        attempt_id: Uuid,
    },
}

struct PendingDiscovery {
    client: GrpcMcpFederationClient,
    command: RunExecutionCommand,
    purpose: McpDiscoveryPurpose,
}

/// Applies concurrent MCP network results through the sole mutable Kernel
/// owner. Message transports may publish or acknowledge the returned outcome,
/// but cannot interleave catalog attachment with a Run transition.
pub struct McpDiscoveryCoordinator {
    supervisor: McpDiscoverySupervisor,
    pending: HashMap<Uuid, PendingDiscovery>,
}

impl McpDiscoveryCoordinator {
    #[must_use]
    pub fn new(buffer_capacity: usize) -> Self {
        Self {
            supervisor: McpDiscoverySupervisor::new(buffer_capacity),
            pending: HashMap::new(),
        }
    }

    pub fn start(
        &mut self,
        processor: &WorkerProcessor,
        client: GrpcMcpFederationClient,
        attempt_id: Uuid,
    ) -> Result<bool, WorkerAssignmentError> {
        let execution = processor
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if self.pending.contains_key(&attempt_id) {
            return Ok(false);
        }
        let command = execution.command.clone();
        let cancellation = execution.cancellation.clone();
        let purpose = if execution.restored_from_checkpoint.is_some() {
            McpDiscoveryPurpose::Restore
        } else {
            McpDiscoveryPurpose::Start
        };
        if !self.supervisor.start(
            processor.tool_registry().clone(),
            client.clone(),
            command.clone(),
            cancellation,
        ) {
            return Ok(false);
        }
        self.pending.insert(
            attempt_id,
            PendingDiscovery {
                client,
                command,
                purpose,
            },
        );
        Ok(true)
    }

    pub async fn recv_and_apply(
        &mut self,
        processor: &mut WorkerProcessor,
        timeout: Duration,
    ) -> Result<Option<McpDiscoveryCompletion>, WorkerAssignmentError> {
        let Some(update) = self.supervisor.recv(timeout).await else {
            return Ok(None);
        };
        let attempt_id = match &update {
            McpDiscoveryUpdate::Ready { attempt_id, .. }
            | McpDiscoveryUpdate::Cancelled { attempt_id } => *attempt_id,
        };
        let pending = self
            .pending
            .remove(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        match update {
            McpDiscoveryUpdate::Ready { discovered, .. } => {
                let mcp_servers = discovered.statuses.clone();
                let attached = attach_discovered_federated_tools(
                    processor,
                    pending.client,
                    &pending.command,
                    attempt_id,
                    *discovered,
                );
                match attached {
                    Ok(()) => {}
                    Err(WorkerAssignmentError::RequiredMcpServersUnavailable(_)) => {
                        let failed_servers = mcp_servers
                            .iter()
                            .filter(|status| {
                                status.required && status.health == McpServerHealth::Unavailable
                            })
                            .map(|status| status.server_name.clone())
                            .collect::<Vec<_>>();
                        let event = processor
                            .record_required_mcp_unavailable(attempt_id, &failed_servers)?;
                        return Ok(Some(McpDiscoveryCompletion::Failed {
                            attempt_id,
                            event,
                            mcp_servers,
                        }));
                    }
                    Err(error) => return Err(error),
                }
                match pending.purpose {
                    McpDiscoveryPurpose::Start => {
                        let event = processor.start(attempt_id)?;
                        Ok(Some(McpDiscoveryCompletion::Started {
                            attempt_id,
                            event,
                            mcp_servers,
                        }))
                    }
                    McpDiscoveryPurpose::Restore => {
                        processor.verify_restored_federated_tools(attempt_id)?;
                        Ok(Some(McpDiscoveryCompletion::Restored {
                            attempt_id,
                            mcp_servers,
                        }))
                    }
                }
            }
            McpDiscoveryUpdate::Cancelled { .. } => {
                Ok(Some(McpDiscoveryCompletion::Cancelled { attempt_id }))
            }
        }
    }
}
