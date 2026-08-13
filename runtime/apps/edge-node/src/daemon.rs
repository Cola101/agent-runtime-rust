use crate::transport::{EdgeOutboundConfig, EdgeOutboundConnector};
use crate::{
    EdgeCapabilityManifest, EdgeControlPlaneTrust, EdgeDeviceIdentity, EdgeNode, EdgeNodeError,
    EdgeNodeStore, verify_edge_enrollment_grant,
};
use agent_grpc_security::ClientMtlsMaterials;
use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDaemonFileConfig {
    schema_version: u32,
    edge_state_root: PathBuf,
    enrollment_grant_path: PathBuf,
    control_plane_public_keys: BTreeMap<String, String>,
    control_plane_endpoint: String,
    tls: EdgeDaemonTlsConfig,
    capability_manifest: EdgeCapabilityManifest,
    profiles: Vec<EdgeDaemonRuntimeProfileConfig>,
    #[serde(default)]
    admission: EdgeDaemonAdmissionConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDaemonRuntimeProfileConfig {
    runtime_state_root: PathBuf,
    workspace_root: PathBuf,
    invocation: RuntimeInvocationContext,
    agent_instructions: String,
    provider: EdgeDaemonProviderConfig,
    budget: RunBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDaemonTlsConfig {
    client_certificate_path: PathBuf,
    client_private_key_path: PathBuf,
    server_ca_path: PathBuf,
    server_domain_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDaemonProviderConfig {
    id: String,
    protocol: String,
    endpoint: String,
    model: String,
    api_key_path: PathBuf,
    region: String,
    accepted_data_classes: BTreeSet<DataClass>,
    capabilities: BTreeSet<Capability>,
    latency_ms: u64,
    cost_per_million_tokens_micros: u64,
    #[serde(default = "default_provider_timeout_ms")]
    response_timeout_ms: u64,
    #[serde(default = "default_provider_timeout_ms")]
    stream_idle_timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeDaemonAdmissionConfig {
    max_active_runs: usize,
    max_active_runs_per_tenant: usize,
    max_active_runs_per_workspace: usize,
    max_queued_runs: usize,
    max_queued_runs_per_tenant: usize,
}

impl Default for EdgeDaemonAdmissionConfig {
    fn default() -> Self {
        Self {
            max_active_runs: 8,
            max_active_runs_per_tenant: 4,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 64,
            max_queued_runs_per_tenant: 16,
        }
    }
}

const fn default_provider_timeout_ms() -> u64 {
    60_000
}

pub struct EdgeDaemon {
    connector: EdgeOutboundConnector,
    node_id: Uuid,
    node_generation: u64,
    profile_count: usize,
}

impl std::fmt::Debug for EdgeDaemon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EdgeDaemon")
            .field("node_id", &self.node_id)
            .field("node_generation", &self.node_generation)
            .field("profile_count", &self.profile_count)
            .finish_non_exhaustive()
    }
}

impl EdgeDaemon {
    pub fn from_config_file(
        path: impl AsRef<Path>,
        now_unix_ms: i64,
    ) -> Result<Self, EdgeNodeError> {
        let body = std::fs::read(path.as_ref())
            .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
        let config = serde_json::from_slice::<EdgeDaemonFileConfig>(&body).map_err(|error| {
            EdgeNodeError::InvalidOutboundConfigurationWithDetail(error.to_string())
        })?;
        if config.schema_version != 1
            || !config.edge_state_root.is_absolute()
            || config.profiles.is_empty()
            || config.profiles.len() > 256
        {
            return Err(EdgeNodeError::InvalidOutboundConfiguration);
        }
        for profile in &config.profiles {
            validate_profile(profile)?;
        }
        let trust = decode_trust_set(config.control_plane_public_keys)?;
        let identity = EdgeDeviceIdentity::load_or_create(&config.edge_state_root)?;
        let enrollment_token = read_owner_only_text(&config.enrollment_grant_path)?;
        let enrollment = verify_edge_enrollment_grant(
            &enrollment_token,
            &trust,
            &identity,
            &config.capability_manifest,
            now_unix_ms,
        )?;
        let node_id = enrollment.claims().node_id;
        let node_generation = enrollment.claims().node_generation;
        let mut runtime_profiles = Vec::with_capacity(config.profiles.len());
        for profile in config.profiles {
            runtime_profiles.push(build_runtime_profile(profile)?);
        }
        let profile_count = runtime_profiles.len();
        let runtime = EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: config.admission.max_active_runs,
                max_active_runs_per_tenant: config.admission.max_active_runs_per_tenant,
                max_active_runs_per_workspace: config.admission.max_active_runs_per_workspace,
                max_queued_runs: config.admission.max_queued_runs,
                max_queued_runs_per_tenant: config.admission.max_queued_runs_per_tenant,
            },
            runtime_profiles,
        )
        .map_err(|error| {
            EdgeNodeError::InvalidOutboundConfigurationWithDetail(error.to_string())
        })?;
        let store = EdgeNodeStore::open_enrolled(&config.edge_state_root, &enrollment)?;
        let node = Arc::new(EdgeNode::new(enrollment, trust, store, runtime)?);
        validate_owner_only_file(&config.tls.client_private_key_path)?;
        let tls = ClientMtlsMaterials::from_files(
            config.tls.client_certificate_path,
            config.tls.client_private_key_path,
            config.tls.server_ca_path,
            config.tls.server_domain_name,
        )
        .map_err(|error| {
            EdgeNodeError::InvalidOutboundConfigurationWithDetail(error.to_string())
        })?;
        let outbound = EdgeOutboundConfig::new(config.control_plane_endpoint, tls)?;
        Ok(Self {
            connector: EdgeOutboundConnector::new(identity, node, outbound),
            node_id,
            node_generation,
            profile_count,
        })
    }

    #[must_use]
    pub const fn node_id(&self) -> Uuid {
        self.node_id
    }

    #[must_use]
    pub const fn node_generation(&self) -> u64 {
        self.node_generation
    }

    #[must_use]
    pub const fn profile_count(&self) -> usize {
        self.profile_count
    }

    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), EdgeNodeError> {
        self.connector.run(shutdown).await
    }
}

fn validate_profile(profile: &EdgeDaemonRuntimeProfileConfig) -> Result<(), EdgeNodeError> {
    if !profile.runtime_state_root.is_absolute()
        || !profile.workspace_root.is_absolute()
        || profile.agent_instructions.trim().is_empty()
        || profile.provider.id.trim().is_empty()
        || profile.provider.endpoint.trim().is_empty()
        || profile.provider.model.trim().is_empty()
        || profile.provider.region.trim().is_empty()
        || profile.provider.accepted_data_classes.is_empty()
        || profile.provider.capabilities.is_empty()
        || profile.provider.response_timeout_ms == 0
        || profile.provider.stream_idle_timeout_ms == 0
        || ProviderProtocol::from_str(&profile.provider.protocol).is_err()
    {
        return Err(EdgeNodeError::InvalidOutboundConfiguration);
    }
    profile
        .invocation
        .validate()
        .map_err(|_| EdgeNodeError::InvalidOutboundConfiguration)
}

fn build_runtime_profile(
    profile: EdgeDaemonRuntimeProfileConfig,
) -> Result<RuntimeProfile, EdgeNodeError> {
    let api_key = read_owner_only_text(&profile.provider.api_key_path)?;
    let protocol = ProviderProtocol::from_str(&profile.provider.protocol)
        .map_err(|_| EdgeNodeError::InvalidOutboundConfiguration)?;
    let data_class = least_sensitive_class(&profile.provider.accepted_data_classes);
    let provider_region = profile.provider.region.clone();
    let max_cost = profile.provider.cost_per_million_tokens_micros;
    Ok(RuntimeProfile {
        invocation: profile.invocation,
        config: LocalRuntimeConfig {
            state_root: profile.runtime_state_root,
            workspace_root: profile.workspace_root,
            agent_instructions: profile.agent_instructions,
            delegated_scopes: BTreeSet::new(),
            subagent_roles: Vec::new(),
            model_routing: LocalModelRoutingConfig {
                candidates: vec![LocalProviderConfig {
                    id: profile.provider.id,
                    protocol,
                    endpoint: profile.provider.endpoint,
                    model: profile.provider.model,
                    api_key,
                    region: provider_region.clone(),
                    accepted_data_classes: profile.provider.accepted_data_classes,
                    capabilities: profile.provider.capabilities,
                    healthy: true,
                    latency_ms: profile.provider.latency_ms,
                    cost_per_million_tokens_micros: max_cost,
                    response_timeout_ms: profile.provider.response_timeout_ms,
                    stream_idle_timeout_ms: profile.provider.stream_idle_timeout_ms,
                }],
                allowed_regions: BTreeSet::from([provider_region]),
                data_class,
                max_cost_per_million_tokens_micros: max_cost,
                health_policy: Default::default(),
            },
            mcp_servers: Vec::new(),
            mcp_lifecycle: LocalMcpLifecycleConfig::default(),
            trusted_workspace_tool: None,
            process_session: None,
            consent: LocalToolConsent::Ask,
            budget: profile.budget,
            runtime_policy: RuntimeExecutionPolicySnapshot::default(),
        },
    })
}

fn decode_trust_set(
    encoded: BTreeMap<String, String>,
) -> Result<EdgeControlPlaneTrust, EdgeNodeError> {
    let mut keys = BTreeMap::new();
    for (key_id, value) in encoded {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| EdgeNodeError::InvalidTrustSet)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| EdgeNodeError::InvalidTrustSet)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| EdgeNodeError::InvalidTrustSet)?;
        keys.insert(key_id, key);
    }
    EdgeControlPlaneTrust::new(keys)
}

fn read_owner_only_text(path: &Path) -> Result<String, EdgeNodeError> {
    validate_owner_only_file(path)?;
    let value =
        std::fs::read_to_string(path).map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    let value = value.trim();
    if value.is_empty() || value.len() > 64 * 1024 {
        return Err(EdgeNodeError::InvalidOutboundConfiguration);
    }
    Ok(value.into())
}

fn validate_owner_only_file(path: &Path) -> Result<(), EdgeNodeError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EdgeNodeError::InvalidOutboundConfiguration);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(EdgeNodeError::InvalidOutboundConfiguration);
        }
    }
    Ok(())
}

fn least_sensitive_class(classes: &BTreeSet<DataClass>) -> DataClass {
    classes
        .iter()
        .next()
        .copied()
        .unwrap_or(DataClass::Restricted)
}
