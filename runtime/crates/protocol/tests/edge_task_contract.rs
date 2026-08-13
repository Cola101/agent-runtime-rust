use agent_protocol::{
    EDGE_TASK_SCHEMA_VERSION, EdgeTaskClaims, EdgeTaskValidationError,
    RUNTIME_INVOCATION_SCHEMA_VERSION, RuntimeInvocationContext,
};
use std::collections::BTreeSet;
use uuid::Uuid;

fn invocation() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
        tenant_id: Uuid::from_u128(1),
        application_id: Uuid::from_u128(2),
        workload_identity_id: Uuid::from_u128(3),
        workspace_id: Uuid::from_u128(4),
        agent_version_id: Uuid::from_u128(5),
        model_policy_id: Uuid::from_u128(6),
    }
}

fn task() -> EdgeTaskClaims {
    let run_id = Uuid::from_u128(9);
    EdgeTaskClaims {
        schema_version: EDGE_TASK_SCHEMA_VERSION,
        task_id: Uuid::from_u128(7),
        enrollment_id: Uuid::from_u128(70),
        node_id: Uuid::from_u128(8),
        node_generation: 3,
        capability_manifest_digest: "a".repeat(64),
        required_capabilities: BTreeSet::from(["runtime.agent.execute".into()]),
        issued_at_unix_ms: 1_000,
        expires_at_unix_ms: 61_000,
        invocation: invocation(),
        run_id,
        session_id: run_id,
        workspace_owner_epoch: 11,
        input: "inspect the registered workspace".into(),
    }
}

/// The production break this catches is treating an absent Enrollment or a
/// malformed capability requirement as a wildcard. A signed task must remain
/// bound to one approved node surface before any Runtime admission occurs.
#[test]
fn edge_task_requires_enrollment_and_bounded_capabilities() {
    assert_eq!(task().validate_at(2_000), Ok(()));

    let mut missing_enrollment = task();
    missing_enrollment.enrollment_id = Uuid::nil();
    assert!(missing_enrollment.validate_at(2_000).is_err());

    let mut bad_digest = task();
    bad_digest.capability_manifest_digest = "not-a-digest".into();
    assert!(bad_digest.validate_at(2_000).is_err());

    let mut no_capability = task();
    no_capability.required_capabilities.clear();
    assert!(no_capability.validate_at(2_000).is_err());

    let mut malformed_capability = task();
    malformed_capability.required_capabilities = BTreeSet::from([" runtime.execute".into()]);
    assert!(malformed_capability.validate_at(2_000).is_err());

    let mut too_many = task();
    too_many.required_capabilities = (0..65)
        .map(|index| format!("runtime.cap.{index}"))
        .collect();
    assert!(too_many.validate_at(2_000).is_err());
}

/// The production break this catches is letting a v1 edge task name a Session
/// that the one-shot embedded Runtime does not actually execute. That would
/// sign one identity while the Runtime durably records another.
#[test]
fn v1_edge_task_rejects_a_session_that_is_not_the_run_identity() {
    let mut claims = task();
    claims.session_id = Uuid::from_u128(10);

    assert_eq!(
        claims.validate_at(2_000),
        Err(EdgeTaskValidationError::UnsupportedSessionIdentity)
    );
}

/// The production break this catches is replaying an offline task after its
/// signed authority expired, or accepting a task whose validity window is so
/// large that revocation cannot be made effective.
#[test]
fn edge_task_authority_has_a_bounded_live_window() {
    let mut expired = task();
    expired.expires_at_unix_ms = 2_000;
    assert!(expired.validate_at(2_000).is_err());

    let mut overlong = task();
    overlong.expires_at_unix_ms = overlong.issued_at_unix_ms + 24 * 60 * 60 * 1_000 + 1;
    assert!(overlong.validate_at(2_000).is_err());

    let mut overflowing = task();
    overflowing.issued_at_unix_ms = i64::MIN;
    overflowing.expires_at_unix_ms = i64::MAX;
    assert!(overflowing.validate_at(0).is_err());
}

/// The production break this catches is treating an absent node, task,
/// Workspace owner or invocation component as a wildcard at the edge boundary.
#[test]
fn edge_task_requires_complete_identity_and_positive_fences() {
    assert_eq!(task().validate_at(2_000), Ok(()));

    let mut variants = Vec::new();
    let mut value = task();
    value.task_id = Uuid::nil();
    variants.push(value);
    let mut value = task();
    value.node_id = Uuid::nil();
    variants.push(value);
    let mut value = task();
    value.node_generation = 0;
    variants.push(value);
    let mut value = task();
    value.workspace_owner_epoch = 0;
    variants.push(value);
    let mut value = task();
    value.invocation.workspace_id = Uuid::nil();
    variants.push(value);

    for invalid in variants {
        assert!(invalid.validate_at(2_000).is_err(), "accepted {invalid:?}");
    }
}

/// The production break this catches is allowing a signed task to bypass the
/// standalone Runtime's input bounds and consume unbounded durable storage.
#[test]
fn edge_task_input_is_nonblank_and_bounded_before_execution() {
    let mut blank = task();
    blank.input = "   ".into();
    assert!(blank.validate_at(2_000).is_err());

    let mut oversized = task();
    oversized.input = "x".repeat(32_001);
    assert!(oversized.validate_at(2_000).is_err());
}
