//! Transport-neutral multi-tenant embedding surface.
//!
//! A Java SDK, sidecar, edge node, or desktop process can authenticate a caller
//! outside this crate and then select one pre-registered invocation profile.
//! Requests never carry filesystem paths or provider credentials, so an
//! untrusted client cannot turn identity fields into access to another
//! Workspace.

use crate::admission::{
    RuntimeAdmissionController, RuntimeAdmissionError, RuntimeAdmissionLimits,
    RuntimeAdmissionSnapshot,
};
use crate::{
    LocalEvent, LocalRunOutcome, LocalRunRecord, LocalRuntimeConfig, LocalRuntimeError,
    LocalRuntimeHost,
};
use agent_protocol::RuntimeInvocationContext;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct RuntimeProfile {
    pub invocation: RuntimeInvocationContext,
    pub config: LocalRuntimeConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EmbeddedRuntimeError {
    #[error("embedded Runtime configuration is invalid: {0}")]
    Configuration(String),
    #[error("Runtime invocation is not registered")]
    UnregisteredInvocation,
    #[error(transparent)]
    Admission(#[from] RuntimeAdmissionError),
    #[error(transparent)]
    Runtime(#[from] LocalRuntimeError),
}

/// One process can host many immutable tenant/application/workspace profiles,
/// while the admission controller applies a shared global ceiling and a tenant
/// ceiling before constructing a Host.
pub struct EmbeddedRuntime {
    profiles: HashMap<RuntimeInvocationContext, LocalRuntimeConfig>,
    admission: Arc<RuntimeAdmissionController>,
}

impl EmbeddedRuntime {
    pub fn new(
        limits: RuntimeAdmissionLimits,
        profiles: Vec<RuntimeProfile>,
    ) -> Result<Self, EmbeddedRuntimeError> {
        if profiles.is_empty() {
            return Err(EmbeddedRuntimeError::Configuration(
                "at least one invocation profile is required".into(),
            ));
        }
        let admission = Arc::new(RuntimeAdmissionController::new(limits)?);
        let mut by_identity = HashMap::with_capacity(profiles.len());
        let mut boundaries = HashMap::<(Uuid, Uuid, Uuid), (PathBuf, PathBuf)>::new();
        let mut state_root_owners = HashMap::<PathBuf, (Uuid, Uuid, Uuid)>::new();
        let mut workspace_root_owners = HashMap::<PathBuf, (Uuid, Uuid, Uuid)>::new();
        for mut profile in profiles {
            profile.invocation.validate().map_err(|error| {
                EmbeddedRuntimeError::Configuration(format!(
                    "invocation profile is invalid: {error}"
                ))
            })?;
            if !profile.config.state_root.is_absolute() {
                return Err(EmbeddedRuntimeError::Configuration(
                    "each profile state root must be absolute".into(),
                ));
            }
            std::fs::create_dir_all(&profile.config.state_root).map_err(|error| {
                EmbeddedRuntimeError::Configuration(format!(
                    "each profile state root must be creatable: {error}"
                ))
            })?;
            let state_root =
                std::fs::canonicalize(&profile.config.state_root).map_err(|error| {
                    EmbeddedRuntimeError::Configuration(format!(
                        "each profile state root must resolve to a real directory: {error}"
                    ))
                })?;
            let state_metadata = std::fs::metadata(&state_root).map_err(|error| {
                EmbeddedRuntimeError::Configuration(format!(
                    "each profile state root must be inspectable: {error}"
                ))
            })?;
            if !state_metadata.is_dir() {
                return Err(EmbeddedRuntimeError::Configuration(
                    "each profile state root must be a directory".into(),
                ));
            }
            profile.config.state_root = state_root;
            let workspace =
                std::fs::canonicalize(&profile.config.workspace_root).map_err(|error| {
                    EmbeddedRuntimeError::Configuration(format!(
                        "workspace root cannot be resolved: {error}"
                    ))
                })?;
            let boundary = (
                profile.invocation.tenant_id,
                profile.invocation.application_id,
                profile.invocation.workspace_id,
            );
            let roots = (profile.config.state_root.clone(), workspace.clone());
            if let Some(registered) = boundaries.get(&boundary) {
                if registered != &roots {
                    return Err(EmbeddedRuntimeError::Configuration(
                        "one Workspace identity must use one stable pair of persistent roots"
                            .into(),
                    ));
                }
            } else {
                for (owners, root, label) in [
                    (&state_root_owners, &roots.0, "state root"),
                    (&workspace_root_owners, &roots.1, "workspace root"),
                ] {
                    if owners
                        .get(root)
                        .is_some_and(|registered| registered != &boundary)
                    {
                        return Err(EmbeddedRuntimeError::Configuration(format!(
                            "{label} is owned by another Workspace identity"
                        )));
                    }
                }
                state_root_owners.insert(roots.0.clone(), boundary);
                workspace_root_owners.insert(roots.1.clone(), boundary);
                boundaries.insert(boundary, roots);
            }
            if by_identity
                .insert(profile.invocation, profile.config)
                .is_some()
            {
                return Err(EmbeddedRuntimeError::Configuration(
                    "duplicate invocation profile".into(),
                ));
            }
        }
        Ok(Self {
            profiles: by_identity,
            admission,
        })
    }

    #[must_use]
    pub fn admission_snapshot(&self) -> RuntimeAdmissionSnapshot {
        self.admission.snapshot()
    }

    pub async fn execute(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalRunOutcome, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?.clone();
        let _permit = self.admission.acquire(invocation).await?;
        let mut host = LocalRuntimeHost::start_for_invocation(config, invocation)?;
        let outcome = host.execute_as(run_id, input).await;
        host.shutdown().await;
        outcome.map_err(Into::into)
    }

    pub async fn execute_at_epoch(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?.clone();
        let _permit = self.admission.acquire(invocation).await?;
        let mut host = LocalRuntimeHost::start_for_invocation(config, invocation)?;
        let outcome = host.execute_as_at_epoch(run_id, input, owner_epoch).await;
        host.shutdown().await;
        outcome.map_err(Into::into)
    }

    pub async fn resume(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?.clone();
        let _permit = self.admission.acquire(invocation).await?;
        let mut host = LocalRuntimeHost::start_for_invocation(config, invocation)?;
        let outcome = host.resume(run_id, input, owner_epoch).await;
        host.shutdown().await;
        outcome.map_err(Into::into)
    }

    pub fn replay_events(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        after_sequence: u64,
    ) -> Result<Vec<LocalEvent>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        LocalRuntimeHost::replay_events(&config.state_root, run_id, after_sequence)
            .map_err(Into::into)
    }

    pub fn read_run_record(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
    ) -> Result<Option<LocalRunRecord>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        LocalRuntimeHost::read_run_record(&config.state_root, run_id).map_err(Into::into)
    }

    fn profile(
        &self,
        invocation: RuntimeInvocationContext,
    ) -> Result<&LocalRuntimeConfig, EmbeddedRuntimeError> {
        invocation
            .validate()
            .map_err(|error| EmbeddedRuntimeError::Configuration(error.to_string()))?;
        self.profiles
            .get(&invocation)
            .ok_or(EmbeddedRuntimeError::UnregisteredInvocation)
    }
}
