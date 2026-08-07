use agent_protocol::ToolExecutionRequest;
use agent_tool_runtime::{
    ToolExecutionContext, ToolExecutionError, ToolExecutionResult, ToolExecutor,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq)]
pub enum ToolExecutionUpdate {
    Finished {
        attempt_id: Uuid,
        tool_call_id: String,
        binding_digest: String,
        result: ToolExecutionResult,
    },
    Failed {
        attempt_id: Uuid,
        tool_call_id: String,
        binding_digest: String,
        error: ToolExecutionError,
    },
}

pub struct ToolExecutionSupervisor {
    updates: mpsc::Sender<ToolExecutionUpdate>,
    receiver: mpsc::Receiver<ToolExecutionUpdate>,
    launched: HashSet<(Uuid, String)>,
}

impl ToolExecutionSupervisor {
    #[must_use]
    pub fn new(buffer_capacity: usize) -> Self {
        assert!(buffer_capacity > 0, "tool update buffer must be positive");
        let (updates, receiver) = mpsc::channel(buffer_capacity);
        Self {
            updates,
            receiver,
            launched: HashSet::new(),
        }
    }

    pub fn start(
        &mut self,
        executor: Arc<dyn ToolExecutor>,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> bool {
        let identity = (context.attempt_id, request.call.id.clone());
        if !self.launched.insert(identity) {
            return false;
        }
        let updates = self.updates.clone();
        tokio::spawn(async move {
            let attempt_id = context.attempt_id;
            let tool_call_id = request.call.id.clone();
            let binding_digest = request.binding_digest.clone();
            let update = match executor.execute(request, context).await {
                Ok(result) => ToolExecutionUpdate::Finished {
                    attempt_id,
                    tool_call_id,
                    binding_digest,
                    result,
                },
                Err(error) => ToolExecutionUpdate::Failed {
                    attempt_id,
                    tool_call_id,
                    binding_digest,
                    error,
                },
            };
            let _ = updates.send(update).await;
        });
        true
    }

    pub async fn recv(&mut self, timeout: Duration) -> Option<ToolExecutionUpdate> {
        if timeout.is_zero() {
            return None;
        }
        tokio::time::timeout(timeout, self.receiver.recv())
            .await
            .ok()
            .flatten()
    }
}
