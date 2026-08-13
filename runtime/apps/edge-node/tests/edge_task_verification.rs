use agent_edge_node::{
    EdgeControlPlaneTrust, verify_edge_task_token, verify_edge_task_token_for_enrollment,
};
use agent_protocol::{
    EDGE_TASK_SCHEMA_VERSION, EdgeTaskClaims, RUNTIME_INVOCATION_SCHEMA_VERSION,
    RuntimeInvocationContext,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod common;

const KEY_ID: &str = "control-2026-08";

fn claims(now: i64) -> EdgeTaskClaims {
    let run_id = Uuid::from_u128(109);
    EdgeTaskClaims {
        schema_version: EDGE_TASK_SCHEMA_VERSION,
        task_id: Uuid::from_u128(107),
        enrollment_id: Uuid::from_u128(110),
        node_id: Uuid::from_u128(108),
        node_generation: 3,
        capability_manifest_digest: "a".repeat(64),
        required_capabilities: BTreeSet::from(["runtime.agent.execute".into()]),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now + 60_000,
        invocation: RuntimeInvocationContext {
            schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
            tenant_id: Uuid::from_u128(101),
            application_id: Uuid::from_u128(102),
            workload_identity_id: Uuid::from_u128(103),
            workspace_id: Uuid::from_u128(104),
            agent_version_id: Uuid::from_u128(105),
            model_policy_id: Uuid::from_u128(106),
        },
        run_id,
        session_id: run_id,
        workspace_owner_epoch: 17,
        input: "return the registered workspace identity".into(),
    }
}

fn sign(key: &SigningKey, claims: &EdgeTaskClaims) -> String {
    sign_with_key_id(key, KEY_ID, claims)
}

fn sign_with_key_id(key: &SigningKey, key_id: &str, claims: &EdgeTaskClaims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("serialize claims"));
    let signed = format!("edge-task-v1.{key_id}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());
    format!("{signed}.{signature}")
}

/// The production break this catches is accepting a payload that was modified
/// after the control plane signed it. The expected claims are independent
/// literals; the verifier cannot manufacture the assertion from its own data.
#[test]
fn a_tampered_edge_task_is_rejected_before_node_execution() {
    let now = Utc::now().timestamp_millis();
    let key = SigningKey::from_bytes(&[41; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
        .expect("trust set");
    let valid = sign(&key, &claims(now));
    let mut parts = valid.split('.').map(str::to_owned).collect::<Vec<_>>();
    let mut changed = claims(now);
    changed.invocation.workspace_id = Uuid::from_u128(999);
    parts[2] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&changed).expect("serialize tamper"));
    let tampered = parts.join(".");

    assert!(verify_edge_task_token(&tampered, &trust, Uuid::from_u128(108), 3, now).is_err());
}

/// The production break this catches is letting a task addressed to a retired
/// node incarnation execute after reconnect or re-enrollment.
#[test]
fn a_task_for_an_old_node_generation_is_fenced() {
    let now = Utc::now().timestamp_millis();
    let key = SigningKey::from_bytes(&[42; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
        .expect("trust set");
    let token = sign(&key, &claims(now));

    assert!(verify_edge_task_token(&token, &trust, Uuid::from_u128(108), 4, now).is_err());
}

/// The production break this catches is pinning the node to one control-plane
/// key. A bounded trust set must keep in-flight tasks verifiable during key
/// rotation without trusting an unregistered key id.
#[test]
fn the_control_plane_trust_set_supports_key_rotation() {
    let now = Utc::now().timestamp_millis();
    let old = SigningKey::from_bytes(&[43; 32]);
    let new = SigningKey::from_bytes(&[44; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([
        (KEY_ID.into(), old.verifying_key()),
        ("control-2026-09".into(), new.verifying_key()),
    ]))
    .expect("rotating trust set");

    assert!(
        verify_edge_task_token(
            &sign(&old, &claims(now)),
            &trust,
            Uuid::from_u128(108),
            3,
            now
        )
        .is_ok()
    );
    assert!(
        verify_edge_task_token(
            &sign_with_key_id(&new, "control-2026-09", &claims(now)),
            &trust,
            Uuid::from_u128(108),
            3,
            now
        )
        .is_ok()
    );
}

/// The production break this catches is treating a valid control-plane task
/// signature as sufficient authority after the node was re-enrolled or when
/// the task requests a capability outside the approved device surface.
#[test]
fn task_authority_is_bound_to_the_active_enrollment_and_approved_capabilities() {
    let now = Utc::now().timestamp_millis();
    let device_state = tempfile::tempdir().expect("device state");
    let enrollment = common::verified_enrollment(
        device_state.path(),
        Uuid::from_u128(110),
        Uuid::from_u128(108),
        3,
        now,
    );
    let key = SigningKey::from_bytes(&[45; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
        .expect("trust set");

    let mut valid = claims(now);
    valid.capability_manifest_digest = enrollment.claims().capability_manifest_digest.clone();
    assert!(
        verify_edge_task_token_for_enrollment(&sign(&key, &valid), &trust, &enrollment, now)
            .is_ok()
    );

    let mut wrong_enrollment = valid.clone();
    wrong_enrollment.enrollment_id = Uuid::from_u128(999);
    assert!(
        verify_edge_task_token_for_enrollment(
            &sign(&key, &wrong_enrollment),
            &trust,
            &enrollment,
            now
        )
        .is_err()
    );

    let mut unapproved = valid;
    unapproved
        .required_capabilities
        .insert("runtime.shell.unrestricted".into());
    assert!(
        verify_edge_task_token_for_enrollment(&sign(&key, &unapproved), &trust, &enrollment, now)
            .is_err()
    );

    let after_grant_expiry = now + 24 * 60 * 60 * 1_000;
    let mut after_expiry = claims(after_grant_expiry);
    after_expiry.enrollment_id = enrollment.claims().enrollment_id;
    after_expiry.capability_manifest_digest =
        enrollment.claims().capability_manifest_digest.clone();
    assert!(
        verify_edge_task_token_for_enrollment(
            &sign(&key, &after_expiry),
            &trust,
            &enrollment,
            after_grant_expiry,
        )
        .is_err(),
        "an unexpired task token must not extend an expired Enrollment grant"
    );
}
