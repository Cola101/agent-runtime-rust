use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

const TOKEN_VERSION: &str = "v2";
const MAX_TOKEN_LIFETIME_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkloadIdentityClaims {
    pub schema_version: u32,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub model_policy_id: Uuid,
    #[serde(default)]
    pub model_policy_digest: String,
    pub audiences: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

impl WorkloadIdentityClaims {
    #[must_use]
    pub fn authorizes(&self, binding: &WorkloadIdentityBinding) -> bool {
        self.tenant_id == binding.tenant_id
            && self.run_id == binding.run_id
            && self.attempt_id == binding.attempt_id
            && self.worker_id == binding.worker_id
            && self.worker_incarnation_id == binding.worker_incarnation_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadIdentityBinding {
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
}

impl From<&WorkloadIdentityClaims> for WorkloadIdentityBinding {
    fn from(claims: &WorkloadIdentityClaims) -> Self {
        Self {
            tenant_id: claims.tenant_id,
            run_id: claims.run_id,
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
        let valid_policy_binding = match claims.schema_version {
            2 => claims.model_policy_digest.is_empty(),
            3 => {
                claims.model_policy_digest.len() == 64
                    && claims
                        .model_policy_digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }
            _ => false,
        };
        if !valid_policy_binding
            || (required.require_incarnation && claims.worker_incarnation_id.is_nil())
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
