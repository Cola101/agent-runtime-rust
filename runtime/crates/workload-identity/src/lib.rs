use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const TOKEN_VERSION: &str = "v2";
const MAX_TOKEN_LIFETIME_MS: i64 = 5 * 60 * 1000;

/// Schema version carried by an administrative identity that is not a Run.
///
/// Public because a service that exposes an operator surface has to be able to
/// insist on it. Accepting a Run-shaped token there would put the separation
/// between administering and executing back on scope alone, which is what this
/// version exists to replace.
pub const OPERATOR_SCHEMA_VERSION: u32 = 5;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkloadIdentityClaims {
    pub schema_version: u32,
    pub tenant_id: Uuid,
    #[serde(default)]
    pub application_id: Uuid,
    #[serde(default)]
    pub workload_identity_id: Uuid,
    pub run_id: Uuid,
    #[serde(default)]
    pub session_id: Uuid,
    #[serde(default)]
    pub workspace_id: Uuid,
    #[serde(default)]
    pub agent_version_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub model_policy_id: Uuid,
    #[serde(default)]
    pub model_policy_digest: String,
    /// Exact MCP Server snapshots this token may present to the credential
    /// gateway, keyed by `server_id`. Empty means federation is not delegated.
    #[serde(default)]
    pub authorized_mcp_servers: BTreeMap<Uuid, String>,
    pub audiences: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

impl WorkloadIdentityClaims {
    /// Whether these claims describe an operator rather than an execution.
    #[must_use]
    pub fn is_operator(&self) -> bool {
        self.schema_version == OPERATOR_SCHEMA_VERSION
    }

    #[must_use]
    pub fn authorizes(&self, binding: &WorkloadIdentityBinding) -> bool {
        self.tenant_id == binding.tenant_id
            && self.application_id == binding.application_id
            && self.workload_identity_id == binding.workload_identity_id
            && self.run_id == binding.run_id
            && self.session_id == binding.session_id
            && self.workspace_id == binding.workspace_id
            && self.agent_version_id == binding.agent_version_id
            && self.attempt_id == binding.attempt_id
            && self.worker_id == binding.worker_id
            && self.worker_incarnation_id == binding.worker_incarnation_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadIdentityBinding {
    pub tenant_id: Uuid,
    pub application_id: Uuid,
    pub workload_identity_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
}

impl From<&WorkloadIdentityClaims> for WorkloadIdentityBinding {
    fn from(claims: &WorkloadIdentityClaims) -> Self {
        Self {
            tenant_id: claims.tenant_id,
            application_id: claims.application_id,
            workload_identity_id: claims.workload_identity_id,
            run_id: claims.run_id,
            session_id: claims.session_id,
            workspace_id: claims.workspace_id,
            agent_version_id: claims.agent_version_id,
            attempt_id: claims.attempt_id,
            worker_id: claims.worker_id,
            worker_incarnation_id: claims.worker_incarnation_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequiredCapability<'a> {
    audience: &'a str,
    scope: &'a str,
    require_incarnation: bool,
}

impl<'a> RequiredCapability<'a> {
    #[must_use]
    pub const fn new(audience: &'a str, scope: &'a str, require_incarnation: bool) -> Self {
        Self {
            audience,
            scope,
            require_incarnation,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkloadTokenError {
    #[error("invalid workload token format")]
    InvalidFormat,
    #[error("invalid workload token signature")]
    InvalidSignature,
    #[error("invalid workload token claims")]
    InvalidClaims,
    #[error("workload token does not grant the required capability")]
    MissingCapability,
    #[error("workload token is expired or has an invalid lifetime")]
    InvalidLifetime,
}

#[derive(Clone)]
pub struct WorkloadTokenVerifier {
    verifying_key: VerifyingKey,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkloadTokenVerifierConfigurationError {
    #[error("workload identity public key is not valid Base64")]
    InvalidBase64,
    #[error("workload identity public key must contain exactly 32 bytes")]
    InvalidLength,
    #[error("workload identity public key is invalid")]
    InvalidKey,
}

impl WorkloadTokenVerifier {
    #[must_use]
    pub const fn new(verifying_key: VerifyingKey) -> Self {
        Self { verifying_key }
    }

    pub fn from_base64(encoded: &str) -> Result<Self, WorkloadTokenVerifierConfigurationError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| WorkloadTokenVerifierConfigurationError::InvalidBase64)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| WorkloadTokenVerifierConfigurationError::InvalidLength)?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| WorkloadTokenVerifierConfigurationError::InvalidKey)?;
        Ok(Self::new(key))
    }

    pub fn verify(
        &self,
        token: &str,
        required: RequiredCapability<'_>,
        now_unix_ms: i64,
    ) -> Result<WorkloadIdentityClaims, WorkloadTokenError> {
        let mut parts = token.split('.');
        let version = parts.next();
        let payload = parts.next();
        let signature = parts.next();
        if version != Some(TOKEN_VERSION)
            || payload.is_none()
            || signature.is_none()
            || parts.next().is_some()
        {
            return Err(WorkloadTokenError::InvalidFormat);
        }
        let payload = payload.expect("checked above");
        let signature = URL_SAFE_NO_PAD
            .decode(signature.expect("checked above"))
            .map_err(|_| WorkloadTokenError::InvalidSignature)?;
        let signature =
            Signature::from_slice(&signature).map_err(|_| WorkloadTokenError::InvalidSignature)?;
        self.verifying_key
            .verify_strict(format!("{TOKEN_VERSION}.{payload}").as_bytes(), &signature)
            .map_err(|_| WorkloadTokenError::InvalidSignature)?;
        let claims = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| WorkloadTokenError::InvalidClaims)?;
        let claims = serde_json::from_slice::<WorkloadIdentityClaims>(&claims)
            .map_err(|_| WorkloadTokenError::InvalidClaims)?;
        let legacy_invocation_fields_are_empty = claims.application_id.is_nil()
            && claims.workload_identity_id.is_nil()
            && claims.session_id.is_nil()
            && claims.workspace_id.is_nil()
            && claims.agent_version_id.is_nil()
            && claims.authorized_mcp_servers.is_empty();
        let valid_policy_binding = match claims.schema_version {
            2 => claims.model_policy_digest.is_empty() && legacy_invocation_fields_are_empty,
            3 => is_sha256(&claims.model_policy_digest) && legacy_invocation_fields_are_empty,
            4 => {
                is_sha256(&claims.model_policy_digest)
                    && !claims.application_id.is_nil()
                    && !claims.workload_identity_id.is_nil()
                    && !claims.session_id.is_nil()
                    && !claims.workspace_id.is_nil()
                    && !claims.agent_version_id.is_nil()
                    && claims.authorized_mcp_servers.len() <= 32
                    && claims
                        .authorized_mcp_servers
                        .iter()
                        .all(|(server_id, digest)| !server_id.is_nil() && is_sha256(digest))
            }
            // Schema 5 is an operator identity, which is deliberately not a Run.
            // It names who is acting and for which tenant, and nothing about an
            // execution. Requiring every Run-scoped field to be absent is what
            // makes the separation structural: a Run token can never satisfy an
            // operator binding, because its run_id is populated and the
            // operator binding's is not. Before this, administering and
            // federating were told apart only by scope, and one token shape
            // could carry either.
            OPERATOR_SCHEMA_VERSION => {
                claims.model_policy_digest.is_empty()
                    && claims.run_id.is_nil()
                    && claims.attempt_id.is_nil()
                    && claims.worker_id.is_nil()
                    && claims.worker_incarnation_id.is_nil()
                    && claims.model_policy_id.is_nil()
                    && claims.session_id.is_nil()
                    && claims.workspace_id.is_nil()
                    && claims.agent_version_id.is_nil()
                    && claims.authorized_mcp_servers.is_empty()
                    && !claims.application_id.is_nil()
                    && !claims.workload_identity_id.is_nil()
            }
            _ => false,
        };
        let operator = claims.schema_version == OPERATOR_SCHEMA_VERSION;
        let run_identity_missing = claims.run_id.is_nil()
            || claims.attempt_id.is_nil()
            || claims.worker_id.is_nil()
            || claims.model_policy_id.is_nil();
        if claims.tenant_id.is_nil()
            || !valid_policy_binding
            // A Run token must name its execution; an operator token must not.
            || (!operator && run_identity_missing)
            || (!operator && required.require_incarnation && claims.worker_incarnation_id.is_nil())
        {
            return Err(WorkloadTokenError::InvalidClaims);
        }
        if claims.issued_at_unix_ms > now_unix_ms
            || claims.expires_at_unix_ms <= now_unix_ms
            || claims.expires_at_unix_ms - claims.issued_at_unix_ms > MAX_TOKEN_LIFETIME_MS
        {
            return Err(WorkloadTokenError::InvalidLifetime);
        }
        if !claims.audiences.contains(required.audience) || !claims.scopes.contains(required.scope)
        {
            return Err(WorkloadTokenError::MissingCapability);
        }
        Ok(claims)
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
