use agent_edge_node::{
    EdgeControlPlaneTrust, EdgeEnrollmentRevocationClaims, EdgeNodeStore, EdgeOutboxAckClaims,
    EdgeRuntimeEvent, EdgeTaskReceipt, EdgeTaskReceiptStatus, VerifiedEdgeTask,
};
use agent_protocol::{
    EDGE_TASK_SCHEMA_VERSION, EdgeTaskClaims, RUNTIME_INVOCATION_SCHEMA_VERSION,
    RuntimeInvocationContext,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer as _, SigningKey};
use sha2::Digest as _;
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

mod common;

fn open_store(path: &Path) -> Result<EdgeNodeStore, agent_edge_node::EdgeNodeError> {
    let enrollment =
        common::verified_enrollment(path, Uuid::from_u128(81), Uuid::from_u128(8), 3, 2_000);
    EdgeNodeStore::open_enrolled(path, &enrollment)
}

fn signed_ack(
    records: &[agent_edge_node::EdgeOutboxRecord],
    through_sequence: u64,
    session_id: Uuid,
    now: i64,
) -> (String, EdgeControlPlaneTrust, String) {
    let batch_digest = hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(records).expect("canonical batch"),
    ));
    let key = SigningKey::from_bytes(&[91; 32]);
    let claims = EdgeOutboxAckClaims {
        schema_version: 1,
        ack_id: Uuid::from_u128(902),
        session_id,
        enrollment_id: Uuid::from_u128(81),
        node_id: Uuid::from_u128(8),
        node_generation: 3,
        through_sequence,
        batch_digest: batch_digest.clone(),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now + 60_000,
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("ACK claims"));
    let signed = format!("edge-outbox-ack-v1.ack-control-2026-08.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        "ack-control-2026-08".into(),
        key.verifying_key(),
    )]))
    .expect("trust");
    (format!("{signed}.{signature}"), trust, batch_digest)
}

fn receipt(verified: &VerifiedEdgeTask, last_runtime_sequence: u64) -> EdgeTaskReceipt {
    EdgeTaskReceipt {
        schema_version: 1,
        task_id: verified.claims.task_id,
        task_digest: verified.task_digest.clone(),
        enrollment_id: verified.claims.enrollment_id,
        capability_manifest_digest: verified.claims.capability_manifest_digest.clone(),
        node_id: verified.claims.node_id,
        node_generation: verified.claims.node_generation,
        invocation: verified.claims.invocation,
        run_id: verified.claims.run_id,
        session_id: verified.claims.session_id,
        workspace_owner_epoch: verified.claims.workspace_owner_epoch,
        status: EdgeTaskReceiptStatus::Succeeded,
        output: "done".into(),
        last_runtime_sequence,
    }
}

fn runtime_event(verified: &VerifiedEdgeTask, sequence: u64) -> EdgeRuntimeEvent {
    use sha2::{Digest as _, Sha256};

    let payload = serde_json::json!({"text": "edge"});
    EdgeRuntimeEvent {
        schema_version: 1,
        task_id: verified.claims.task_id,
        task_digest: verified.task_digest.clone(),
        enrollment_id: verified.claims.enrollment_id,
        capability_manifest_digest: verified.claims.capability_manifest_digest.clone(),
        node_id: verified.claims.node_id,
        node_generation: verified.claims.node_generation,
        invocation: verified.claims.invocation,
        workspace_owner_epoch: verified.claims.workspace_owner_epoch,
        event_id: Uuid::from_u128(20 + u128::from(sequence)),
        session_id: verified.claims.session_id,
        run_id: verified.claims.run_id,
        sequence,
        attempt_id: Uuid::from_u128(30),
        timestamp: chrono::Utc::now(),
        trace_id: Uuid::from_u128(31),
        event_type: "model.output.delta".into(),
        digest: hex::encode(Sha256::digest(
            serde_json::to_vec(&payload).expect("payload"),
        )),
        payload,
    }
}

fn task(task_id: u128, run_id: u128, digest: &str) -> VerifiedEdgeTask {
    let run_id = Uuid::from_u128(run_id);
    VerifiedEdgeTask {
        claims: EdgeTaskClaims {
            schema_version: EDGE_TASK_SCHEMA_VERSION,
            task_id: Uuid::from_u128(task_id),
            enrollment_id: Uuid::from_u128(81),
            node_id: Uuid::from_u128(8),
            node_generation: 3,
            capability_manifest_digest: common::capability_manifest()
                .digest()
                .expect("manifest digest"),
            required_capabilities: std::collections::BTreeSet::from([
                "runtime.agent.execute".into()
            ]),
            issued_at_unix_ms: 1_000,
            expires_at_unix_ms: 61_000,
            invocation: RuntimeInvocationContext {
                schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
                tenant_id: Uuid::from_u128(1),
                application_id: Uuid::from_u128(2),
                workload_identity_id: Uuid::from_u128(3),
                workspace_id: Uuid::from_u128(4),
                agent_version_id: Uuid::from_u128(5),
                model_policy_id: Uuid::from_u128(6),
            },
            run_id,
            session_id: run_id,
            workspace_owner_epoch: 11,
            input: "edge input".into(),
        },
        signing_key_id: "control-2026-08".into(),
        task_digest: digest.into(),
    }
}

fn task_at_epoch(task_id: u128, run_id: u128, digest: &str, epoch: u64) -> VerifiedEdgeTask {
    let mut task = task(task_id, run_id, digest);
    task.claims.workspace_owner_epoch = epoch;
    task
}

/// The production break this catches is accepting a duplicate delivery as new
/// work after process restart. The persisted task receipt, not an in-memory
/// set, must decide whether model and Tool execution may begin.
#[test]
fn a_reserved_task_is_not_reserved_again_after_store_restart() {
    let state = tempfile::tempdir().expect("edge state");
    let first = open_store(state.path()).expect("first store");
    let verified = task(7, 9, &"a".repeat(64));
    assert!(first.reserve(&verified).expect("reserve").is_new());
    drop(first);

    let replacement = open_store(state.path()).expect("replacement store");
    let duplicate = replacement.reserve(&verified).expect("duplicate lookup");
    assert!(!duplicate.is_new());
    assert_eq!(duplicate.receipt().status, EdgeTaskReceiptStatus::Accepted);
}

/// The production break this catches is reusing one idempotency key for a
/// different signed task or Run and receiving the first task's cached result.
#[test]
fn a_task_id_cannot_be_rebound_to_another_signed_payload() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    store
        .reserve(&task(7, 9, &"a".repeat(64)))
        .expect("first reserve");

    assert!(store.reserve(&task(7, 10, &"b".repeat(64))).is_err());
}

/// The production break this catches is accepting a terminal receipt from a
/// different Enrollment after the task was reserved. That would let a newer or
/// cloned node generation take ownership of an older side effect ledger.
#[test]
fn a_reserved_task_cannot_be_completed_under_another_enrollment() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    let verified = task(7, 9, &"a".repeat(64));
    store.reserve(&verified).expect("reserve");
    let mut forged = receipt(&verified, 0);
    forged.enrollment_id = Uuid::from_u128(999);

    assert!(store.complete(forged).is_err());
}

/// The production break this catches is signing a second task ID for an
/// already reserved Run. The Runtime would otherwise execute that Run again
/// before duplicate event sequences reveal the collision.
#[test]
fn a_run_id_cannot_be_rebound_to_another_task_id() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    store
        .reserve(&task(7, 9, &"a".repeat(64)))
        .expect("first reserve");

    assert!(store.reserve(&task(8, 9, &"b".repeat(64))).is_err());
}

/// The production break this catches is accepting an older signed owner after
/// a newer Workspace owner already executed on the same node. Every new Run
/// must respect a durable per-Workspace epoch high-water mark.
#[test]
fn an_older_workspace_owner_epoch_is_fenced_before_reservation() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    store
        .reserve(&task_at_epoch(7, 9, &"a".repeat(64), 12))
        .expect("new owner reserve");

    assert!(
        store
            .reserve(&task_at_epoch(8, 10, &"b".repeat(64), 11))
            .is_err()
    );
}

/// The production break this catches is letting a transport adapter advance
/// the durable cursor from a naked integer or replay an ACK from another
/// connection. Pruning is authorized only by a control-plane signature bound
/// to the exact uploaded batch and active Enrollment.
#[test]
fn outbox_pruning_requires_a_session_and_batch_bound_control_plane_ack() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    store
        .reserve(&task(7, 9, &"a".repeat(64)))
        .expect("reserve");
    let records = store.pending_outbox(0, 10).expect("pending outbox");
    assert_eq!(records.len(), 1);
    let session_id = Uuid::from_u128(901);
    let now = 50_000;
    let (valid_ack, trust, batch_digest) = signed_ack(&records, 1, session_id, now);

    let (wrong_session, _, _) = signed_ack(&records, 1, Uuid::from_u128(999), now);
    assert!(
        store
            .apply_signed_outbox_ack(&wrong_session, &trust, session_id, &batch_digest, now,)
            .is_err()
    );
    assert_eq!(store.pending_outbox(0, 10).expect("retained").len(), 1);

    store
        .apply_signed_outbox_ack(&valid_ack, &trust, session_id, &batch_digest, now)
        .expect("authorized ACK");
    assert!(store.pending_outbox(0, 10).expect("pruned").is_empty());
}

/// The production break this catches is acknowledging an upload cursor beyond
/// what the node has durably emitted, which would permanently lose events.
#[test]
fn outbox_ack_is_monotonic_bounded_and_survives_restart() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    let verified = task(7, 9, &"a".repeat(64));
    store.reserve(&verified).expect("reserve");
    store.complete(receipt(&verified, 0)).expect("complete");
    let pending = store.pending_outbox(0, 10).expect("pending outbox");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].sequence, 1);
    assert_eq!(pending[1].sequence, 2);
    let session_id = Uuid::from_u128(903);
    let now = 60_000;
    let (beyond, beyond_trust, beyond_digest) = signed_ack(&pending, 3, session_id, now);
    assert!(
        store
            .apply_signed_outbox_ack(&beyond, &beyond_trust, session_id, &beyond_digest, now,)
            .is_err()
    );
    let (first_ack, trust, digest) = signed_ack(&pending[..1], 1, session_id, now);
    store
        .apply_signed_outbox_ack(&first_ack, &trust, session_id, &digest, now)
        .expect("ack first");
    drop(store);

    let replacement = open_store(state.path()).expect("replacement");
    let remaining = replacement.pending_outbox(1, 10).expect("resume outbox");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].sequence, 2);
    let (regression, trust, digest) = signed_ack(&remaining, 0, session_id, now);
    assert!(
        replacement
            .apply_signed_outbox_ack(&regression, &trust, session_id, &digest, now)
            .is_err()
    );
}

/// The production break this catches is accepting more work after the control
/// plane revoked the active Enrollment, or forgetting the revocation on
/// restart. Revocation must be signed, exact-generation-bound and durable.
#[test]
fn signed_enrollment_revocation_persists_and_blocks_same_generation_reopen() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    let now = 70_000;
    let key = SigningKey::from_bytes(&[92; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        "revoke-control-2026-08".into(),
        key.verifying_key(),
    )]))
    .expect("trust");
    let claims = EdgeEnrollmentRevocationClaims {
        schema_version: 1,
        revocation_id: Uuid::from_u128(910),
        enrollment_id: Uuid::from_u128(81),
        device_id: common::verified_enrollment(
            state.path(),
            Uuid::from_u128(81),
            Uuid::from_u128(8),
            3,
            2_000,
        )
        .claims()
        .device_id,
        node_id: Uuid::from_u128(8),
        node_generation: 3,
        reason_code: "operator_revoked".into(),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now + 60_000,
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("revocation claims"));
    let signed = format!("edge-enrollment-revocation-v1.revoke-control-2026-08.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());

    store
        .apply_signed_enrollment_revocation(&format!("{signed}.{signature}"), &trust, now)
        .expect("durable revocation");
    drop(store);

    assert!(open_store(state.path()).is_err());
}

/// The production break this catches is two daemon processes both loading the
/// same snapshot and atomically replacing each other's updates. File-level
/// atomicity does not provide single-writer ownership by itself.
#[test]
fn one_state_root_has_exactly_one_live_edge_node_writer() {
    let state = tempfile::tempdir().expect("edge state");
    let first = open_store(state.path()).expect("first writer");
    assert!(open_store(state.path()).is_err());
    drop(first);
    open_store(state.path()).expect("replacement writer after release");
}

/// The production break this catches is accepting a durable outbox snapshot
/// after an unacknowledged record was truncated. A reconnect would then advance
/// past a silent hole and permanently lose a receipt or Runtime event.
#[test]
fn an_unacknowledged_outbox_gap_is_rejected_on_restart() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    store
        .reserve(&task(7, 9, &"a".repeat(64)))
        .expect("reserve");
    drop(store);

    let state_file = state.path().join("edge-node-state.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_file).expect("read durable snapshot"))
            .expect("parse durable snapshot");
    snapshot["outbox"] = serde_json::json!([]);
    std::fs::write(
        &state_file,
        serde_json::to_vec(&snapshot).expect("serialize damaged snapshot"),
    )
    .expect("damage durable snapshot");

    assert!(open_store(state.path()).is_err());
}

/// The production break this catches is committing Runtime event 2 without
/// event 1. Outbox record numbers may still be contiguous, so the Store must
/// independently enforce the Runtime sequence carried by each event.
#[test]
fn a_runtime_event_gap_cannot_be_committed_as_a_complete_receipt() {
    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    let verified = task(7, 9, &"a".repeat(64));
    store.reserve(&verified).expect("reserve");

    assert!(
        store
            .complete_with_events(receipt(&verified, 2), &[runtime_event(&verified, 2)])
            .is_err()
    );
}

/// The production break this catches is allowing one provider or Tool event to
/// grow the atomic JSON outbox without a per-record bound, exhausting node
/// memory and disk before the count limit can help.
#[test]
fn an_oversized_runtime_event_is_rejected_before_it_enters_the_outbox() {
    use sha2::{Digest as _, Sha256};

    let state = tempfile::tempdir().expect("edge state");
    let store = open_store(state.path()).expect("store");
    let verified = task(7, 9, &"a".repeat(64));
    store.reserve(&verified).expect("reserve");
    let mut event = runtime_event(&verified, 1);
    event.payload = serde_json::json!({"text": "x".repeat(1024 * 1024 + 1)});
    event.digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&event.payload).expect("payload"),
    ));

    assert!(
        store
            .complete_with_events(receipt(&verified, 1), &[event])
            .is_err()
    );
}
