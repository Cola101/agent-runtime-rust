use agent_runtime_worker::{WorkerIdentityError, load_or_create_worker_id};
use std::fs;

#[test]
fn persisted_worker_identity_survives_process_restarts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker-id");

    let first = load_or_create_worker_id(&path).unwrap();
    let second = load_or_create_worker_id(&path).unwrap();

    assert_eq!(first, second);
    assert_eq!(fs::read_to_string(path).unwrap(), first.to_string());
}

#[test]
fn corrupt_persisted_worker_identity_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker-id");
    fs::write(&path, "not-a-uuid").unwrap();

    let error = load_or_create_worker_id(&path).unwrap_err();
    assert!(matches!(error, WorkerIdentityError::Invalid { .. }));
}
