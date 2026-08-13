use crate::{FederatedRunTools, GrpcMcpFederationClient, discover_federated_tools};
use agent_kernel::ToolRegistry;
use agent_protocol::RunExecutionCommand;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// One terminal network-side discovery outcome. The caller remains the sole
/// owner of mutable Kernel state and decides when to attach the catalog.
pub enum McpDiscoveryUpdate {
    Ready {
        attempt_id: Uuid,
        discovered: FederatedRunTools,
    },
    Cancelled {
        attempt_id: Uuid,
    },
}

impl McpDiscoveryUpdate {
    fn attempt_id(&self) -> Uuid {
        match self {
            Self::Ready { attempt_id, .. } | Self::Cancelled { attempt_id } => *attempt_id,
        }
    }
}

/// Runs MCP network discovery concurrently while keeping result application
/// outside the tasks. This separation lets a native Host admit other Runs
/// without allowing network completion order to mutate Kernel state.
pub struct McpDiscoverySupervisor {
    updates: mpsc::Sender<McpDiscoveryUpdate>,
    receiver: mpsc::Receiver<McpDiscoveryUpdate>,
    launched: HashSet<Uuid>,
}

impl McpDiscoverySupervisor {
    #[must_use]
    pub fn new(buffer_capacity: usize) -> Self {
        assert!(
            buffer_capacity > 0,
            "MCP discovery update buffer must be positive"
        );
        let (updates, receiver) = mpsc::channel(buffer_capacity);
        Self {
            updates,
            receiver,
            launched: HashSet::new(),
        }
    }

    pub fn start(
        &mut self,
        registry: ToolRegistry,
        mut client: GrpcMcpFederationClient,
        command: RunExecutionCommand,
        cancellation: CancellationToken,
    ) -> bool {
        let attempt_id = command.attempt_id;
        if !self.launched.insert(attempt_id) {
            return false;
        }
        let updates = self.updates.clone();
        tokio::spawn(async move {
            let workload_token = command.workload_token.as_str().to_owned();
            let discovery =
                discover_federated_tools(&registry, &mut client, &command, &workload_token);
            let update = tokio::select! {
                biased;
                () = cancellation.cancelled() => McpDiscoveryUpdate::Cancelled { attempt_id },
                discovered = discovery => McpDiscoveryUpdate::Ready {
                    attempt_id,
                    discovered,
                },
            };
            let _ = updates.send(update).await;
        });
        true
    }

    pub async fn recv(&mut self, timeout: Duration) -> Option<McpDiscoveryUpdate> {
        if timeout.is_zero() {
            return None;
        }
        let update = tokio::time::timeout(timeout, self.receiver.recv())
            .await
            .ok()
            .flatten()?;
        self.launched.remove(&update.attempt_id());
        Some(update)
    }
}
