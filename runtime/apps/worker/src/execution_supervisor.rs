use crate::{GrpcModelGatewayClient, ModelGatewayClientError, PreparedModelInvocation};
use agent_protocol::{ModelErrorKind, ModelStreamEvent};
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq)]
pub enum ModelExecutionUpdate {
    AuthenticationRequired {
        attempt_id: Uuid,
    },
    Event {
        attempt_id: Uuid,
        event: ModelStreamEvent,
    },
    Finished {
        attempt_id: Uuid,
    },
    Cancelled {
        attempt_id: Uuid,
    },
}

pub struct ModelExecutionSupervisor {
    updates: mpsc::Sender<ModelExecutionUpdate>,
    receiver: mpsc::Receiver<ModelExecutionUpdate>,
    launched: HashSet<Uuid>,
}

impl ModelExecutionSupervisor {
    #[must_use]
    pub fn new(buffer_capacity: usize) -> Self {
        assert!(buffer_capacity > 0, "model update buffer must be positive");
        let (updates, receiver) = mpsc::channel(buffer_capacity);
        Self {
            updates,
            receiver,
            launched: HashSet::new(),
        }
    }

    pub fn start(
        &mut self,
        attempt_id: Uuid,
        mut client: GrpcModelGatewayClient,
        prepared: PreparedModelInvocation,
        cancellation: CancellationToken,
    ) -> bool {
        if !self.launched.insert(attempt_id) {
            return false;
        }
        let updates = self.updates.clone();
        tokio::spawn(async move {
            let (events, mut event_receiver) = mpsc::channel(32);
            let execution = client.execute(
                prepared.invocation,
                prepared.workload_token.as_str(),
                cancellation,
                events,
            );
            tokio::pin!(execution);
            let result = loop {
                tokio::select! {
                    event = event_receiver.recv() => {
                        if let Some(event) = event
                            && updates.send(ModelExecutionUpdate::Event { attempt_id, event }).await.is_err()
                        {
                            return;
                        }
                    }
                    result = &mut execution => {
                        while let Some(event) = event_receiver.recv().await {
                            if updates.send(ModelExecutionUpdate::Event { attempt_id, event }).await.is_err() {
                                return;
                            }
                        }
                        break result;
                    }
                }
            };
            match result {
                Ok(()) => {
                    let _ = updates
                        .send(ModelExecutionUpdate::Finished { attempt_id })
                        .await;
                }
                Err(ModelGatewayClientError::Cancelled) => {
                    let _ = updates
                        .send(ModelExecutionUpdate::Cancelled { attempt_id })
                        .await;
                }
                Err(ModelGatewayClientError::Rpc {
                    code: Code::Unauthenticated,
                    ..
                }) => {
                    if updates
                        .send(ModelExecutionUpdate::AuthenticationRequired { attempt_id })
                        .await
                        .is_ok()
                    {
                        let _ = updates
                            .send(ModelExecutionUpdate::Finished { attempt_id })
                            .await;
                    }
                }
                Err(error) => {
                    let event = classify_failure(error);
                    if updates
                        .send(ModelExecutionUpdate::Event { attempt_id, event })
                        .await
                        .is_ok()
                    {
                        let _ = updates
                            .send(ModelExecutionUpdate::Finished { attempt_id })
                            .await;
                    }
                }
            }
        });
        true
    }

    pub async fn recv(&mut self, timeout: Duration) -> Option<ModelExecutionUpdate> {
        if timeout.is_zero() {
            return None;
        }
        let update = tokio::time::timeout(timeout, self.receiver.recv())
            .await
            .ok()
            .flatten()?;
        if let ModelExecutionUpdate::Finished { attempt_id }
        | ModelExecutionUpdate::Cancelled { attempt_id } = &update
        {
            self.launched.remove(attempt_id);
        }
        Some(update)
    }
}

fn classify_failure(error: ModelGatewayClientError) -> ModelStreamEvent {
    let (kind, retryable) = match &error {
        ModelGatewayClientError::Transport(_) => (ModelErrorKind::Unavailable, true),
        ModelGatewayClientError::Rpc { code, .. } => match code {
            Code::PermissionDenied => (ModelErrorKind::Authentication, false),
            Code::Unauthenticated => unreachable!("unauthenticated is recovered before failure"),
            Code::ResourceExhausted => (ModelErrorKind::RateLimited, true),
            Code::DeadlineExceeded => (ModelErrorKind::Timeout, true),
            Code::Unavailable => (ModelErrorKind::Unavailable, true),
            _ => (ModelErrorKind::Protocol, false),
        },
        ModelGatewayClientError::InvalidEvent(_) | ModelGatewayClientError::ConsumerClosed => {
            (ModelErrorKind::Protocol, false)
        }
        ModelGatewayClientError::Cancelled => unreachable!("cancellation is handled separately"),
    };
    ModelStreamEvent::Failed {
        kind,
        retryable,
        message: error.to_string(),
    }
}
