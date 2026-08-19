use crate::{ProviderAdapter, ProviderCredential, ProviderExecutionError};
use agent_protocol::{
    ModelErrorKind, ModelFailoverPolicySnapshot, ModelRequest, ModelStreamEvent,
    RuntimeExecutionPolicySnapshot,
};
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
    execute_with_frozen_failover(
        routes,
        request,
        &RuntimeExecutionPolicySnapshot::default().model_failover,
        cancellation,
        events,
    )
    .await
}

pub async fn execute_with_frozen_failover(
    routes: &[ProviderRoute],
    request: &ModelRequest,
    policy: &ModelFailoverPolicySnapshot,
    cancellation: CancellationToken,
    events: mpsc::Sender<ModelStreamEvent>,
) -> Result<FailoverSelection, ProviderExecutionError> {
    if routes.is_empty() {
        return Err(ProviderExecutionError::InvalidConfiguration(
            "model policy has no provider candidates".into(),
        ));
    }
    if !(1..=8).contains(&policy.max_provider_attempts)
        || policy.fallback_on.iter().any(|kind| {
            !matches!(
                kind,
                ModelErrorKind::RateLimited
                    | ModelErrorKind::Timeout
                    | ModelErrorKind::Unavailable
                    | ModelErrorKind::Billing
            )
        })
    {
        return Err(ProviderExecutionError::InvalidConfiguration(
            "runtime model failover policy is invalid".into(),
        ));
    }

    let mut failed_provider_ids = Vec::new();
    let routes = &routes[..routes.len().min(usize::from(policy.max_provider_attempts))];
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
        let provider_id = route.id.clone();
        let provider_task = tokio::spawn(async move {
            adapter
                .execute(
                    &provider_id,
                    &model_request,
                    &credential,
                    task_cancellation,
                    attempt_tx,
                )
                .await
        });
        let mut committed_events = 0_u64;
        while let Some(event) = attempt_rx.recv().await {
            committed_events += u64::from(event.commits_provider_output());
            if events.send(event).await.is_err() {
                attempt_cancellation.cancel();
                let _ = provider_task.await;
                return Err(ProviderExecutionError::ConsumerClosed);
            }
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
                if committed_events == 0
                    && index + 1 < routes.len()
                    && is_policy_fallback(&error, policy) =>
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

/// Whether this failure says anything about a *different* provider.
///
/// Two questions were being answered by one flag. `retryable` asks whether the
/// same call will work later; failover asks whether a different provider will
/// work now. For every transient kind the answers coincide, which is why one
/// flag served for both until a failure appeared where they diverge.
///
/// An exhausted quota is that failure. Retrying the same account is pointless,
/// possibly for days -- and a different provider is a different account, so it
/// is exactly the case a second candidate was configured for. Requiring
/// `retryable` here would mean the more truthful classification of a quota 429
/// costs the failover that the *wrong* classification (`RateLimited`) was
/// buying by accident.
fn crosses_to_another_provider(kind: ModelErrorKind, retryable: bool) -> bool {
    retryable || matches!(kind, ModelErrorKind::Billing)
}

fn is_policy_fallback(
    error: &ProviderExecutionError,
    policy: &ModelFailoverPolicySnapshot,
) -> bool {
    matches!(
        error,
        ProviderExecutionError::Provider { kind, retryable, .. }
            if policy.fallback_on.contains(kind)
                && crosses_to_another_provider(*kind, *retryable)
    )
}
