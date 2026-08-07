use agent_protocol::{
    CheckpointPayloadEncoding, CheckpointSnapshot, RUN_CHECKPOINT_SCHEMA_VERSION,
    RunCheckpointPublished, RunCheckpointValidationError, RunStatus,
};
use chrono::Utc;
use sha2::Digest;
use uuid::Uuid;

#[test]
fn inline_checkpoint_must_fit_below_the_default_nats_payload_limit() {
    let snapshot = CheckpointSnapshot::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunStatus::Running,
        7,
        vec![0; 400 * 1024],
    );
    let checkpoint =
        RunCheckpointPublished::new(&snapshot, 1, Uuid::now_v7(), "a".repeat(64), Utc::now());

    assert_eq!(
        checkpoint.validate(),
        Err(RunCheckpointValidationError::InvalidPayload)
    );
}

#[test]
fn v2_compresses_a_large_repetitive_checkpoint_inline() {
    let snapshot = snapshot(vec![b'x'; 900 * 1024]);

    let prepared = RunCheckpointPublished::prepare_v2(
        &snapshot,
        2,
        Uuid::now_v7(),
        "a".repeat(64),
        Utc::now(),
    )
    .unwrap();

    assert_eq!(
        prepared.message.schema_version,
        RUN_CHECKPOINT_SCHEMA_VERSION
    );
    assert_eq!(
        prepared.message.payload_encoding,
        CheckpointPayloadEncoding::Zstd
    );
    assert!(prepared.message.payload_base64.is_some());
    assert!(prepared.message.payload_ref.is_none());
    assert!(prepared.external_payload.is_none());
    assert_eq!(prepared.message.decode_snapshot().unwrap(), snapshot);
}

#[test]
fn v2_moves_a_large_incompressible_checkpoint_to_content_addressed_storage() {
    let mut state = Vec::with_capacity(900 * 1024);
    for index in 0..900 * 1024 {
        state.push(ShaByte::at(index));
    }
    let snapshot = snapshot(state);

    let prepared = RunCheckpointPublished::prepare_v2(
        &snapshot,
        3,
        Uuid::now_v7(),
        "b".repeat(64),
        Utc::now(),
    )
    .unwrap();

    assert!(prepared.message.payload_base64.is_none());
    let payload_ref = prepared.message.payload_ref.as_deref().unwrap();
    assert!(payload_ref.starts_with("checkpoint://sha256/"));
    let stored = prepared.external_payload.as_deref().unwrap();
    assert_eq!(prepared.message.stored_size as usize, stored.len());
    assert_eq!(
        prepared
            .message
            .decode_snapshot_with_payload(stored)
            .unwrap(),
        snapshot
    );
}

#[test]
fn external_checkpoint_rejects_corruption_before_decompression() {
    let snapshot = snapshot((0..900 * 1024).map(ShaByte::at).collect());
    let prepared = RunCheckpointPublished::prepare_v2(
        &snapshot,
        4,
        Uuid::now_v7(),
        "c".repeat(64),
        Utc::now(),
    )
    .unwrap();
    let mut stored = prepared.external_payload.unwrap();
    stored[0] ^= 0xff;

    assert_eq!(
        prepared.message.decode_snapshot_with_payload(&stored),
        Err(RunCheckpointValidationError::InvalidPayload)
    );
}

#[test]
fn checkpoint_snapshot_still_reads_the_legacy_json_byte_array() {
    let snapshot = snapshot(vec![1, 2, 3, 255]);
    let mut legacy = serde_json::to_value(&snapshot).unwrap();
    legacy["state"] = serde_json::json!([1, 2, 3, 255]);

    let restored: CheckpointSnapshot = serde_json::from_value(legacy).unwrap();

    assert_eq!(restored, snapshot);
    assert!(restored.verify_digest());
}

fn snapshot(state: Vec<u8>) -> CheckpointSnapshot {
    CheckpointSnapshot::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunStatus::Running,
        7,
        state,
    )
}

struct ShaByte;

impl ShaByte {
    fn at(index: usize) -> u8 {
        let digest = sha2::Sha256::digest(index.to_le_bytes());
        digest[index % digest.len()]
    }
}
