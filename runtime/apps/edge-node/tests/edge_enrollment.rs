use agent_edge_node::{
    EdgeCapabilityManifest, EdgeControlPlaneTrust, EdgeDeviceIdentity, EdgeEnrollmentGrantClaims,
    EdgeNodeStore, verify_edge_enrollment_grant, verify_edge_enrollment_request,
    verify_edge_session_proof,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod common;

const CONTROL_KEY_ID: &str = "control-enrollment-2026-08";

fn manifest() -> EdgeCapabilityManifest {
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
    .expect("capability manifest")
}

fn sign_grant(key: &SigningKey, claims: &EdgeEnrollmentGrantClaims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("grant claims"));
    let signed = format!("edge-enrollment-grant-v1.{CONTROL_KEY_ID}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());
    format!("{signed}.{signature}")
}

/// The production break this catches is silently replacing a node key after a
/// restart or storing it in a file readable by another local user. Both would
/// invalidate device identity as the root of Enrollment authority.
#[test]
fn device_identity_is_stable_and_owner_only() {
    let state = tempfile::tempdir().expect("device state");
    let first = EdgeDeviceIdentity::load_or_create(state.path()).expect("first identity");
    let first_device_id = first.device_id();
    let first_public_key = first.public_key_base64url().to_owned();
    drop(first);

    let replacement = EdgeDeviceIdentity::load_or_create(state.path()).expect("replacement");
    assert_eq!(replacement.device_id(), first_device_id);
    assert_eq!(replacement.public_key_base64url(), first_public_key);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(state.path().join("edge-device-identity.json"))
            .expect("identity metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// The production break this catches is accepting a copied or tampered
/// Enrollment request without proof that the device owns the advertised key
/// and the one-time control-plane challenge.
#[test]
fn enrollment_request_proves_device_key_challenge_and_declared_capabilities() {
    let state = tempfile::tempdir().expect("device state");
    let identity = EdgeDeviceIdentity::load_or_create(state.path()).expect("identity");
    let challenge_id = Uuid::from_u128(501);
    let nonce = [9_u8; 32];
    let now = 10_000;
    let request = identity
        .create_enrollment_request(challenge_id, &nonce, &manifest(), now, now + 60_000)
        .expect("request");

    let verified = verify_edge_enrollment_request(&request, challenge_id, &nonce, now)
        .expect("verified request");
    assert_eq!(verified.claims().device_id, identity.device_id());
    assert_eq!(verified.claims().capability_manifest, manifest());

    let mut tampered = request.into_bytes();
    let index = tampered.len() / 2;
    tampered[index] ^= 1;
    let tampered = String::from_utf8(tampered).expect("ASCII token");
    assert!(verify_edge_enrollment_request(&tampered, challenge_id, &nonce, now).is_err());
    assert!(
        verify_edge_enrollment_request(
            &identity
                .create_enrollment_request(challenge_id, &nonce, &manifest(), now, now + 60_000)
                .expect("second request"),
            Uuid::from_u128(502),
            &nonce,
            now
        )
        .is_err()
    );
}

/// The production break this catches is booting the Edge Node from process
/// arguments or a grant issued to another device/capability surface. The node
/// must accept only a control-plane signature bound to its durable public key.
#[test]
fn enrollment_grant_binds_device_node_generation_and_approved_surface() {
    let state = tempfile::tempdir().expect("device state");
    let identity = EdgeDeviceIdentity::load_or_create(state.path()).expect("identity");
    let control = SigningKey::from_bytes(&[71; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        CONTROL_KEY_ID.into(),
        control.verifying_key(),
    )]))
    .expect("trust");
    let now = 20_000;
    let claims = EdgeEnrollmentGrantClaims {
        schema_version: 1,
        enrollment_id: Uuid::from_u128(601),
        device_id: identity.device_id(),
        device_public_key_base64url: identity.public_key_base64url().into(),
        node_id: Uuid::from_u128(602),
        node_generation: 4,
        capability_manifest_digest: manifest().digest().expect("manifest digest"),
        approved_capabilities: BTreeSet::from([
            "runtime.agent.execute".into(),
            "runtime.events.replay".into(),
        ]),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now - 1_000 + 24 * 60 * 60 * 1_000,
    };

    let verified = verify_edge_enrollment_grant(
        &sign_grant(&control, &claims),
        &trust,
        &identity,
        &manifest(),
        now,
    )
    .expect("grant");
    assert_eq!(verified.claims().node_id, Uuid::from_u128(602));
    assert_eq!(verified.claims().node_generation, 4);

    let other_state = tempfile::tempdir().expect("other state");
    let other = EdgeDeviceIdentity::load_or_create(other_state.path()).expect("other identity");
    assert!(
        verify_edge_enrollment_grant(
            &sign_grant(&control, &claims),
            &trust,
            &other,
            &manifest(),
            now
        )
        .is_err()
    );

    let mut overprivileged = claims.clone();
    overprivileged
        .approved_capabilities
        .insert("runtime.shell.unrestricted".into());
    assert!(
        verify_edge_enrollment_grant(
            &sign_grant(&control, &overprivileged),
            &trust,
            &identity,
            &manifest(),
            now
        )
        .is_err()
    );

    let mut overlong = claims;
    overlong.expires_at_unix_ms = overlong.issued_at_unix_ms + 24 * 60 * 60 * 1_000 + 1;
    assert!(
        verify_edge_enrollment_grant(
            &sign_grant(&control, &overlong),
            &trust,
            &identity,
            &manifest(),
            now
        )
        .is_err(),
        "an offline enrollment grant must not outlive the 24-hour revocation bound"
    );
}

/// The production break this catches is allowing a stale process generation to
/// reopen the ledger after a control-plane-authorized re-enrollment, or making
/// a legitimate generation rotation discard durable outbox history.
#[test]
fn enrollment_generation_advances_monotonically_on_one_device_ledger() {
    let state = tempfile::tempdir().expect("edge state");
    let now = 30_000;
    let base = common::verified_enrollment(
        state.path(),
        Uuid::from_u128(701),
        Uuid::from_u128(703),
        4,
        now,
    );
    let first = EdgeNodeStore::open_enrolled(state.path(), &base).expect("first generation");
    drop(first);

    let next = common::verified_enrollment(
        state.path(),
        Uuid::from_u128(704),
        Uuid::from_u128(703),
        5,
        now,
    );
    let replacement =
        EdgeNodeStore::open_enrolled(state.path(), &next).expect("authorized next generation");
    drop(replacement);

    assert!(EdgeNodeStore::open_enrolled(state.path(), &base).is_err());

    let other_state = tempfile::tempdir().expect("other device state");
    let other_device = common::verified_enrollment(
        other_state.path(),
        Uuid::from_u128(705),
        Uuid::from_u128(703),
        6,
        now,
    );
    assert!(EdgeNodeStore::open_enrolled(state.path(), &other_device).is_err());
}

/// The production break this catches is treating possession of any trusted
/// mTLS client certificate as possession of this enrolled device. Every live
/// connection must also prove the durable device key against a fresh server
/// challenge and the exact Enrollment grant.
#[test]
fn session_proof_binds_a_fresh_challenge_to_the_enrolled_device_key() {
    let state = tempfile::tempdir().expect("device state");
    let now = 40_000;
    let identity = EdgeDeviceIdentity::load_or_create(state.path()).expect("identity");
    let enrollment = common::verified_enrollment(
        state.path(),
        Uuid::from_u128(801),
        Uuid::from_u128(802),
        5,
        now,
    );
    let session_id = Uuid::from_u128(803);
    let nonce = [83_u8; 32];
    let proof = identity
        .create_session_proof(session_id, &nonce, &enrollment, now, now + 60_000)
        .expect("session proof");

    assert!(verify_edge_session_proof(&proof, &enrollment, session_id, &nonce, now,).is_ok());
    assert!(
        verify_edge_session_proof(&proof, &enrollment, Uuid::from_u128(804), &nonce, now,).is_err()
    );

    let after_grant_expiry = now + 24 * 60 * 60 * 1_000;
    assert!(
        identity
            .create_session_proof(
                Uuid::from_u128(805),
                &nonce,
                &enrollment,
                after_grant_expiry,
                after_grant_expiry + 60_000,
            )
            .is_err(),
        "an expired Enrollment grant must not authorize a fresh live session"
    );
}
