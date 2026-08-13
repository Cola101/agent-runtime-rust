#![allow(dead_code)]

use agent_edge_node::{
    EdgeCapabilityManifest, EdgeControlPlaneTrust, EdgeDeviceIdentity, EdgeEnrollmentGrantClaims,
    VerifiedEdgeEnrollment, verify_edge_enrollment_grant,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer as _, SigningKey};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use uuid::Uuid;

const ENROLLMENT_KEY_ID: &str = "test-enrollment-control-2026-08";

pub fn capability_manifest() -> EdgeCapabilityManifest {
    EdgeCapabilityManifest::new(
        env!("CARGO_PKG_VERSION"),
        "macos",
        "aarch64",
        BTreeSet::from([
            "runtime.agent.execute".into(),
            "runtime.events.replay".into(),
            "runtime.outbox.v1".into(),
        ]),
    )
    .expect("test capability manifest")
}

pub fn verified_enrollment(
    device_state_root: &Path,
    enrollment_id: Uuid,
    node_id: Uuid,
    node_generation: u64,
    now_unix_ms: i64,
) -> VerifiedEdgeEnrollment {
    let identity = EdgeDeviceIdentity::load_or_create(device_state_root).expect("device identity");
    let manifest = capability_manifest();
    let control = SigningKey::from_bytes(&[71; 32]);
    let claims = EdgeEnrollmentGrantClaims {
        schema_version: 1,
        enrollment_id,
        device_id: identity.device_id(),
        device_public_key_base64url: identity.public_key_base64url().into(),
        node_id,
        node_generation,
        capability_manifest_digest: manifest.digest().expect("manifest digest"),
        approved_capabilities: BTreeSet::from([
            "runtime.agent.execute".into(),
            "runtime.events.replay".into(),
            "runtime.outbox.v1".into(),
        ]),
        issued_at_unix_ms: now_unix_ms - 1_000,
        expires_at_unix_ms: now_unix_ms - 1_000 + 24 * 60 * 60 * 1_000,
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("grant claims"));
    let signed = format!("edge-enrollment-grant-v1.{ENROLLMENT_KEY_ID}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(control.sign(signed.as_bytes()).to_bytes());
    let token = format!("{signed}.{signature}");
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        ENROLLMENT_KEY_ID.into(),
        control.verifying_key(),
    )]))
    .expect("enrollment trust");
    verify_edge_enrollment_grant(&token, &trust, &identity, &manifest, now_unix_ms)
        .expect("verified enrollment grant")
}
