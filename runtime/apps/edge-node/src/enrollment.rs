use crate::{EdgeControlPlaneTrust, EdgeNodeError, valid_key_id};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use rand_core::{OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use uuid::Uuid;

const CAPABILITY_MANIFEST_SCHEMA_VERSION: u32 = 1;
const ENROLLMENT_REQUEST_SCHEMA_VERSION: u32 = 1;
const ENROLLMENT_GRANT_SCHEMA_VERSION: u32 = 1;
const DEVICE_IDENTITY_SCHEMA_VERSION: u32 = 1;
const ENROLLMENT_REQUEST_TOKEN_VERSION: &str = "edge-enrollment-request-v1";
const ENROLLMENT_GRANT_TOKEN_VERSION: &str = "edge-enrollment-grant-v1";
const SESSION_PROOF_TOKEN_VERSION: &str = "edge-session-proof-v1";
const MAX_ENROLLMENT_TOKEN_BYTES: usize = 64 * 1024;
const MAX_CAPABILITIES: usize = 64;
const MAX_REQUEST_LIFETIME_MS: i64 = 5 * 60 * 1_000;
const MAX_GRANT_LIFETIME_MS: i64 = 24 * 60 * 60 * 1_000;
const MAX_SESSION_PROOF_LIFETIME_MS: i64 = 2 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeCapabilityManifest {
    pub schema_version: u32,
    pub runtime_version: String,
    pub platform: String,
    pub architecture: String,
    pub capabilities: BTreeSet<String>,
}

impl EdgeCapabilityManifest {
    pub fn new(
        runtime_version: impl Into<String>,
        platform: impl Into<String>,
        architecture: impl Into<String>,
        capabilities: BTreeSet<String>,
    ) -> Result<Self, EdgeNodeError> {
        let manifest = Self {
            schema_version: CAPABILITY_MANIFEST_SCHEMA_VERSION,
            runtime_version: runtime_version.into(),
            platform: platform.into(),
            architecture: architecture.into(),
            capabilities,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn digest(&self) -> Result<String, EdgeNodeError> {
        self.validate()?;
        let encoded =
            serde_json::to_vec(self).map_err(|_| EdgeNodeError::InvalidCapabilityManifest)?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }

    fn validate(&self) -> Result<(), EdgeNodeError> {
        if self.schema_version != CAPABILITY_MANIFEST_SCHEMA_VERSION
            || !valid_label(&self.runtime_version, 64)
            || !valid_label(&self.platform, 32)
            || !valid_label(&self.architecture, 32)
            || self.capabilities.is_empty()
            || self.capabilities.len() > MAX_CAPABILITIES
            || self
                .capabilities
                .iter()
                .any(|capability| !valid_capability(capability))
        {
            return Err(EdgeNodeError::InvalidCapabilityManifest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeEnrollmentRequestClaims {
    pub schema_version: u32,
    pub request_id: Uuid,
    pub challenge_id: Uuid,
    pub challenge_nonce_base64url: String,
    pub device_id: Uuid,
    pub device_public_key_base64url: String,
    pub capability_manifest: EdgeCapabilityManifest,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeEnrollmentGrantClaims {
    pub schema_version: u32,
    pub enrollment_id: Uuid,
    pub device_id: Uuid,
    pub device_public_key_base64url: String,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub capability_manifest_digest: String,
    pub approved_capabilities: BTreeSet<String>,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSessionProofClaims {
    pub schema_version: u32,
    pub proof_id: Uuid,
    pub session_id: Uuid,
    pub challenge_nonce_base64url: String,
    pub enrollment_id: Uuid,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub enrollment_grant_digest: String,
    pub capability_manifest_digest: String,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedEdgeEnrollmentRequest {
    pub(crate) claims: EdgeEnrollmentRequestClaims,
    pub(crate) request_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedEdgeEnrollment {
    pub(crate) claims: EdgeEnrollmentGrantClaims,
    pub(crate) signing_key_id: String,
    pub(crate) grant_digest: String,
}

impl VerifiedEdgeEnrollmentRequest {
    #[must_use]
    pub const fn claims(&self) -> &EdgeEnrollmentRequestClaims {
        &self.claims
    }

    #[must_use]
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }
}

impl VerifiedEdgeEnrollment {
    #[must_use]
    pub const fn claims(&self) -> &EdgeEnrollmentGrantClaims {
        &self.claims
    }

    #[must_use]
    pub fn signing_key_id(&self) -> &str {
        &self.signing_key_id
    }

    #[must_use]
    pub fn grant_digest(&self) -> &str {
        &self.grant_digest
    }
}

pub struct EdgeDeviceIdentity {
    device_id: Uuid,
    public_key_base64url: String,
    signing_key: SigningKey,
}

impl EdgeDeviceIdentity {
    pub fn load_or_create(state_root: impl AsRef<Path>) -> Result<Self, EdgeNodeError> {
        let state_root = state_root.as_ref();
        prepare_identity_root(state_root)?;
        let path = state_root.join("edge-device-identity.json");
        if path.exists() {
            return read_identity(&path);
        }

        let mut secret = [0_u8; 32];
        OsRng.fill_bytes(&mut secret);
        let signing_key = SigningKey::from_bytes(&secret);
        secret.fill(0);
        let public_key_base64url = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let device_id = device_id_for_key(&signing_key.verifying_key());
        let stored = StoredDeviceIdentity {
            schema_version: DEVICE_IDENTITY_SCHEMA_VERSION,
            device_id,
            private_key_base64url: URL_SAFE_NO_PAD.encode(signing_key.to_bytes()),
            public_key_base64url: public_key_base64url.clone(),
        };
        persist_identity_if_absent(state_root, &path, &stored)?;
        read_identity(&path)
    }

    #[must_use]
    pub const fn device_id(&self) -> Uuid {
        self.device_id
    }

    #[must_use]
    pub fn public_key_base64url(&self) -> &str {
        &self.public_key_base64url
    }

    pub fn create_enrollment_request(
        &self,
        challenge_id: Uuid,
        challenge_nonce: &[u8],
        manifest: &EdgeCapabilityManifest,
        issued_at_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<String, EdgeNodeError> {
        if challenge_id.is_nil()
            || !(16..=64).contains(&challenge_nonce.len())
            || !valid_lifetime(
                issued_at_unix_ms,
                expires_at_unix_ms,
                issued_at_unix_ms,
                MAX_REQUEST_LIFETIME_MS,
            )
        {
            return Err(EdgeNodeError::InvalidEnrollmentRequest);
        }
        manifest.validate()?;
        let claims = EdgeEnrollmentRequestClaims {
            schema_version: ENROLLMENT_REQUEST_SCHEMA_VERSION,
            request_id: Uuid::now_v7(),
            challenge_id,
            challenge_nonce_base64url: URL_SAFE_NO_PAD.encode(challenge_nonce),
            device_id: self.device_id,
            device_public_key_base64url: self.public_key_base64url.clone(),
            capability_manifest: manifest.clone(),
            issued_at_unix_ms,
            expires_at_unix_ms,
        };
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&claims).map_err(|_| EdgeNodeError::InvalidEnrollmentRequest)?,
        );
        let signed = format!("{ENROLLMENT_REQUEST_TOKEN_VERSION}.{payload}");
        let signature = URL_SAFE_NO_PAD.encode(self.signing_key.sign(signed.as_bytes()).to_bytes());
        Ok(format!("{signed}.{signature}"))
    }

    pub fn create_session_proof(
        &self,
        session_id: Uuid,
        challenge_nonce: &[u8],
        enrollment: &VerifiedEdgeEnrollment,
        issued_at_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<String, EdgeNodeError> {
        if session_id.is_nil()
            || !(16..=64).contains(&challenge_nonce.len())
            || enrollment.claims.device_id != self.device_id
            || enrollment.claims.device_public_key_base64url != self.public_key_base64url
            || enrollment.claims.issued_at_unix_ms > issued_at_unix_ms
            || enrollment.claims.expires_at_unix_ms <= issued_at_unix_ms
            || expires_at_unix_ms > enrollment.claims.expires_at_unix_ms
            || !valid_lifetime(
                issued_at_unix_ms,
                expires_at_unix_ms,
                issued_at_unix_ms,
                MAX_SESSION_PROOF_LIFETIME_MS,
            )
        {
            return Err(EdgeNodeError::InvalidSessionProof);
        }
        let claims = EdgeSessionProofClaims {
            schema_version: 1,
            proof_id: Uuid::now_v7(),
            session_id,
            challenge_nonce_base64url: URL_SAFE_NO_PAD.encode(challenge_nonce),
            enrollment_id: enrollment.claims.enrollment_id,
            node_id: enrollment.claims.node_id,
            node_generation: enrollment.claims.node_generation,
            enrollment_grant_digest: enrollment.grant_digest.clone(),
            capability_manifest_digest: enrollment.claims.capability_manifest_digest.clone(),
            issued_at_unix_ms,
            expires_at_unix_ms,
        };
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|_| EdgeNodeError::InvalidSessionProof)?);
        let signed = format!("{SESSION_PROOF_TOKEN_VERSION}.{payload}");
        let signature = URL_SAFE_NO_PAD.encode(self.signing_key.sign(signed.as_bytes()).to_bytes());
        Ok(format!("{signed}.{signature}"))
    }
}

pub fn verify_edge_session_proof(
    token: &str,
    enrollment: &VerifiedEdgeEnrollment,
    expected_session_id: Uuid,
    expected_challenge_nonce: &[u8],
    now_unix_ms: i64,
) -> Result<EdgeSessionProofClaims, EdgeNodeError> {
    if token.len() > MAX_ENROLLMENT_TOKEN_BYTES
        || expected_session_id.is_nil()
        || !(16..=64).contains(&expected_challenge_nonce.len())
    {
        return Err(EdgeNodeError::InvalidSessionProof);
    }
    let mut parts = token.split('.');
    let version = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if version != Some(SESSION_PROOF_TOKEN_VERSION)
        || payload.is_none()
        || signature.is_none()
        || parts.next().is_some()
    {
        return Err(EdgeNodeError::InvalidSessionProof);
    }
    let payload = payload.expect("validated above");
    let claims = decode_claims::<EdgeSessionProofClaims>(payload)
        .map_err(|_| EdgeNodeError::InvalidSessionProof)?;
    if claims.schema_version != 1
        || claims.proof_id.is_nil()
        || claims.session_id != expected_session_id
        || claims.challenge_nonce_base64url != URL_SAFE_NO_PAD.encode(expected_challenge_nonce)
        || claims.enrollment_id != enrollment.claims.enrollment_id
        || claims.node_id != enrollment.claims.node_id
        || claims.node_generation != enrollment.claims.node_generation
        || claims.enrollment_grant_digest != enrollment.grant_digest
        || claims.capability_manifest_digest != enrollment.claims.capability_manifest_digest
        || enrollment.claims.issued_at_unix_ms > now_unix_ms
        || enrollment.claims.expires_at_unix_ms <= now_unix_ms
        || claims.issued_at_unix_ms < enrollment.claims.issued_at_unix_ms
        || claims.expires_at_unix_ms > enrollment.claims.expires_at_unix_ms
        || !valid_lifetime(
            claims.issued_at_unix_ms,
            claims.expires_at_unix_ms,
            now_unix_ms,
            MAX_SESSION_PROOF_LIFETIME_MS,
        )
    {
        return Err(EdgeNodeError::InvalidSessionProof);
    }
    let verifying_key = decode_verifying_key(&enrollment.claims.device_public_key_base64url)
        .map_err(|_| EdgeNodeError::InvalidSessionProof)?;
    let signed = format!("{SESSION_PROOF_TOKEN_VERSION}.{payload}");
    verify_signature(
        &verifying_key,
        signed.as_bytes(),
        signature.expect("validated above"),
    )
    .map_err(|_| EdgeNodeError::InvalidSessionProof)?;
    Ok(claims)
}

pub fn verify_edge_enrollment_request(
    token: &str,
    expected_challenge_id: Uuid,
    expected_challenge_nonce: &[u8],
    now_unix_ms: i64,
) -> Result<VerifiedEdgeEnrollmentRequest, EdgeNodeError> {
    if token.len() > MAX_ENROLLMENT_TOKEN_BYTES
        || expected_challenge_id.is_nil()
        || !(16..=64).contains(&expected_challenge_nonce.len())
    {
        return Err(EdgeNodeError::InvalidEnrollmentRequest);
    }
    let mut parts = token.split('.');
    let version = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if version != Some(ENROLLMENT_REQUEST_TOKEN_VERSION)
        || payload.is_none()
        || signature.is_none()
        || parts.next().is_some()
    {
        return Err(EdgeNodeError::InvalidEnrollmentRequest);
    }
    let payload = payload.expect("validated above");
    let claims = decode_claims::<EdgeEnrollmentRequestClaims>(payload)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRequest)?;
    validate_request_claims(&claims, now_unix_ms)?;
    if claims.challenge_id != expected_challenge_id
        || claims.challenge_nonce_base64url != URL_SAFE_NO_PAD.encode(expected_challenge_nonce)
    {
        return Err(EdgeNodeError::InvalidEnrollmentRequest);
    }
    let verifying_key = decode_verifying_key(&claims.device_public_key_base64url)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRequest)?;
    if claims.device_id != device_id_for_key(&verifying_key) {
        return Err(EdgeNodeError::InvalidEnrollmentRequest);
    }
    let signed = format!("{ENROLLMENT_REQUEST_TOKEN_VERSION}.{payload}");
    verify_signature(
        &verifying_key,
        signed.as_bytes(),
        signature.expect("validated above"),
    )
    .map_err(|_| EdgeNodeError::InvalidEnrollmentRequest)?;
    Ok(VerifiedEdgeEnrollmentRequest {
        claims,
        request_digest: hex::encode(Sha256::digest(signed.as_bytes())),
    })
}

pub fn verify_edge_enrollment_grant(
    token: &str,
    trust: &EdgeControlPlaneTrust,
    identity: &EdgeDeviceIdentity,
    manifest: &EdgeCapabilityManifest,
    now_unix_ms: i64,
) -> Result<VerifiedEdgeEnrollment, EdgeNodeError> {
    if token.len() > MAX_ENROLLMENT_TOKEN_BYTES {
        return Err(EdgeNodeError::InvalidEnrollmentGrant);
    }
    let mut parts = token.split('.');
    let version = parts.next();
    let key_id = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if version != Some(ENROLLMENT_GRANT_TOKEN_VERSION)
        || key_id.is_none_or(|value| !valid_key_id(value))
        || payload.is_none()
        || signature.is_none()
        || parts.next().is_some()
    {
        return Err(EdgeNodeError::InvalidEnrollmentGrant);
    }
    let key_id = key_id.expect("validated above");
    let payload = payload.expect("validated above");
    let signed = format!("{ENROLLMENT_GRANT_TOKEN_VERSION}.{key_id}.{payload}");
    let verifying_key = trust
        .verifying_key(key_id)
        .ok_or(EdgeNodeError::UnknownSigningKey)?;
    verify_signature(
        verifying_key,
        signed.as_bytes(),
        signature.expect("validated above"),
    )
    .map_err(|_| EdgeNodeError::InvalidEnrollmentGrant)?;
    let claims = decode_claims::<EdgeEnrollmentGrantClaims>(payload)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentGrant)?;
    validate_grant_claims(&claims, now_unix_ms)?;
    if claims.device_id != identity.device_id
        || claims.device_public_key_base64url != identity.public_key_base64url
    {
        return Err(EdgeNodeError::EnrollmentDeviceMismatch);
    }
    manifest.validate()?;
    if claims.capability_manifest_digest != manifest.digest()?
        || claims.approved_capabilities.is_empty()
        || !claims
            .approved_capabilities
            .is_subset(&manifest.capabilities)
        || !claims
            .approved_capabilities
            .contains("runtime.agent.execute")
    {
        return Err(EdgeNodeError::EnrollmentCapabilityMismatch);
    }
    Ok(VerifiedEdgeEnrollment {
        claims,
        signing_key_id: key_id.into(),
        grant_digest: hex::encode(Sha256::digest(signed.as_bytes())),
    })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDeviceIdentity {
    schema_version: u32,
    device_id: Uuid,
    private_key_base64url: String,
    public_key_base64url: String,
}

fn prepare_identity_root(state_root: &Path) -> Result<(), EdgeNodeError> {
    if let Ok(metadata) = std::fs::symlink_metadata(state_root)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(EdgeNodeError::InvalidNodeIdentity);
    }
    std::fs::create_dir_all(state_root)
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(state_root, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    }
    Ok(())
}

fn persist_identity_if_absent(
    state_root: &Path,
    path: &Path,
    stored: &StoredDeviceIdentity,
) -> Result<(), EdgeNodeError> {
    let body = serde_json::to_vec(stored).map_err(|_| EdgeNodeError::InvalidNodeIdentity)?;
    let staging = state_root.join(format!(".edge-device-{}.partial", Uuid::now_v7()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staging)
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    let write_result = file
        .write_all(&body)
        .and_then(|()| file.sync_all())
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()));
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    match std::fs::hard_link(&staging, path) {
        Ok(()) => {
            std::fs::File::open(state_root)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            return Err(EdgeNodeError::StateIo(error.to_string()));
        }
    }
    std::fs::remove_file(&staging).map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    Ok(())
}

fn read_identity(path: &Path) -> Result<EdgeDeviceIdentity, EdgeNodeError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EdgeNodeError::InvalidNodeIdentity);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(EdgeNodeError::InvalidNodeIdentity);
        }
    }
    let body = std::fs::read(path).map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    let stored = serde_json::from_slice::<StoredDeviceIdentity>(&body)
        .map_err(|_| EdgeNodeError::InvalidNodeIdentity)?;
    if stored.schema_version != DEVICE_IDENTITY_SCHEMA_VERSION {
        return Err(EdgeNodeError::InvalidNodeIdentity);
    }
    let private = URL_SAFE_NO_PAD
        .decode(&stored.private_key_base64url)
        .map_err(|_| EdgeNodeError::InvalidNodeIdentity)?;
    let private: [u8; 32] = private
        .try_into()
        .map_err(|_| EdgeNodeError::InvalidNodeIdentity)?;
    let signing_key = SigningKey::from_bytes(&private);
    let public_key_base64url = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
    if stored.public_key_base64url != public_key_base64url
        || stored.device_id != device_id_for_key(&signing_key.verifying_key())
    {
        return Err(EdgeNodeError::InvalidNodeIdentity);
    }
    Ok(EdgeDeviceIdentity {
        device_id: stored.device_id,
        public_key_base64url,
        signing_key,
    })
}

fn validate_request_claims(
    claims: &EdgeEnrollmentRequestClaims,
    now_unix_ms: i64,
) -> Result<(), EdgeNodeError> {
    claims.capability_manifest.validate()?;
    let nonce = URL_SAFE_NO_PAD
        .decode(&claims.challenge_nonce_base64url)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRequest)?;
    if claims.schema_version != ENROLLMENT_REQUEST_SCHEMA_VERSION
        || claims.request_id.is_nil()
        || claims.challenge_id.is_nil()
        || claims.device_id.is_nil()
        || !(16..=64).contains(&nonce.len())
        || !valid_lifetime(
            claims.issued_at_unix_ms,
            claims.expires_at_unix_ms,
            now_unix_ms,
            MAX_REQUEST_LIFETIME_MS,
        )
    {
        return Err(EdgeNodeError::InvalidEnrollmentRequest);
    }
    Ok(())
}

fn validate_grant_claims(
    claims: &EdgeEnrollmentGrantClaims,
    now_unix_ms: i64,
) -> Result<(), EdgeNodeError> {
    if claims.schema_version != ENROLLMENT_GRANT_SCHEMA_VERSION
        || claims.enrollment_id.is_nil()
        || claims.device_id.is_nil()
        || claims.node_id.is_nil()
        || claims.node_generation == 0
        || !is_sha256(&claims.capability_manifest_digest)
        || claims.approved_capabilities.is_empty()
        || claims.approved_capabilities.len() > MAX_CAPABILITIES
        || claims
            .approved_capabilities
            .iter()
            .any(|capability| !valid_capability(capability))
        || decode_verifying_key(&claims.device_public_key_base64url).is_err()
        || !valid_lifetime(
            claims.issued_at_unix_ms,
            claims.expires_at_unix_ms,
            now_unix_ms,
            MAX_GRANT_LIFETIME_MS,
        )
    {
        return Err(EdgeNodeError::InvalidEnrollmentGrant);
    }
    Ok(())
}

fn valid_lifetime(issued_at: i64, expires_at: i64, now: i64, max_lifetime: i64) -> bool {
    issued_at <= now
        && expires_at > now
        && expires_at > issued_at
        && expires_at
            .checked_sub(issued_at)
            .is_some_and(|lifetime| lifetime <= max_lifetime)
}

fn decode_claims<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn decode_verifying_key(encoded: &str) -> Result<VerifyingKey, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| ())?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| ())?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| ())
}

fn verify_signature(key: &VerifyingKey, signed: &[u8], encoded: &str) -> Result<(), ()> {
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| ())?;
    let signature = Signature::from_slice(&bytes).map_err(|_| ())?;
    key.verify_strict(signed, &signature).map_err(|_| ())
}

fn device_id_for_key(key: &VerifyingKey) -> Uuid {
    let digest = Sha256::digest(key.to_bytes());
    Uuid::from_slice(&digest[..16]).expect("SHA-256 always contains 16 bytes")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_label(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

fn valid_capability(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b':' | b'_' | b'-' | b'/')
        })
}
