use crate::{ProviderAdapter, ProviderCredential, ProviderExecutionError};
use agent_protocol::{ModelErrorKind, ModelRequest, ModelStreamEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct ProviderRoute {
    pub id: String,
    adapter: ProviderAdapter,
    credential: ProviderCredential,
}

impl ProviderRoute {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        adapter: impl Into<ProviderAdapter>,
        credential: ProviderCredential,
    ) -> Self {
        Self {
            id: id.into(),
            adapter: adapter.into(),
            credential,
        }
    }

    #[must_use]
    pub const fn safe_fallback_kinds() -> &'static [ModelErrorKind] {
        &[
            ModelErrorKind::RateLimited,
            ModelErrorKind::Timeout,
            ModelErrorKind::Unavailable,
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailoverSelection {
    pub provider_id: String,
    pub failed_provider_ids: Vec<String>,
}

pub async fn execute_with_safe_failover(
    routes: &[ProviderRoute],
    request: &ModelRequest,
    cancellation: CancellationToken,
    events: mpsc::Sender<ModelStreamEvent>,
) -> Result<FailoverSelection, ProviderExecutionError> {
    if routes.is_empty() {
        return Err(ProviderExecutionError::InvalidConfiguration(
            "model policy has no provider candidates".into(),
        ));
    }

    let mut failed_provider_ids = Vec::new();
    for (index, route) in routes.iter().enumerate() {
        if route.id.trim().is_empty() {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "provider route id must not be blank".into(),
            ));
        }
        let adapter = route.adapter.clone();
        let credential = route.credential.clone();
        let attempt_cancellation = cancellation.child_token();
        let task_cancellation = attempt_cancellation.clone();
        let (attempt_tx, mut attempt_rx) = mpsc::channel(32);
        let model_request = request.clone();
        let provider_task = tokio::spawn(async move {
            adapter
                .execute(&model_request, &credential, task_cancellation, attempt_tx)
                .await
        });
        let mut emitted_events = 0_u64;
        while let Some(event) = attempt_rx.recv().await {
            if events.send(event).await.is_err() {
                attempt_cancellation.cancel();
                let _ = provider_task.await;
                return Err(ProviderExecutionError::ConsumerClosed);
            }
            emitted_events += 1;
        }
        let result = provider_task.await.map_err(|error| {
            ProviderExecutionError::InvalidConfiguration(format!(
                "provider task failed unexpectedly: {error}"
            ))
        })?;
        match result {
            Ok(()) => {
                return Ok(FailoverSelection {
                    provider_id: route.id.clone(),
                    failed_provider_ids,
                });
            }
            Err(error)
                if emitted_events == 0
                    && index + 1 < routes.len()
                    && is_safe_pre_output_failure(&error) =>
            {
                warn!(
                    provider_id = %route.id,
                    next_provider_id = %routes[index + 1].id,
                    "provider failed before output; advancing through the frozen fallback chain"
                );
                failed_provider_ids.push(route.id.clone());
            }
            Err(error) => return Err(error),
        }
    }

    Err(ProviderExecutionError::InvalidConfiguration(
        "model policy exhausted without selecting a provider".into(),
    ))
}

fn is_safe_pre_output_failure(error: &ProviderExecutionError) -> bool {
    matches!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::RateLimited
                | ModelErrorKind::Timeout
                | ModelErrorKind::Unavailable,
            retryable: true,
            ..
        }
    )
}
