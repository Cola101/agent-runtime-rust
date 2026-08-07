use agent_kernel::{RunCommand, RunMachine};
use agent_protocol::RunStatus;
use uuid::Uuid;

#[test]
fn approval_checkpoint_restores_state_and_event_sequence() {
    let mut run = RunMachine::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    run.apply(RunCommand::Start).unwrap();
    run.apply(RunCommand::RequireApproval).unwrap();
    let checkpoint = run.checkpoint(br#"{"cursor":42}"#.to_vec());

    let mut restored = RunMachine::from_checkpoint(checkpoint.clone()).unwrap();
    let resumed = restored.apply(RunCommand::Approve).unwrap();

    assert_eq!(restored.status(), RunStatus::Running);
    assert_eq!(resumed.sequence, 3);
    assert_eq!(checkpoint.state, br#"{"cursor":42}"#);
    assert!(checkpoint.verify_digest());
}

#[test]
fn modified_checkpoint_is_rejected() {
    let run = RunMachine::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let mut checkpoint = run.checkpoint(vec![1, 2, 3]);
    checkpoint.state.push(4);

    assert!(RunMachine::from_checkpoint(checkpoint).is_err());
}

#[test]
fn checkpoint_can_be_rebound_to_a_new_attempt_without_resetting_sequence() {
    let mut run = RunMachine::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    run.apply(RunCommand::Start).unwrap();
    let checkpoint = run.checkpoint(br#"{"cursor":42}"#.to_vec());
    let replacement_attempt_id = Uuid::now_v7();

    let mut restored =
        RunMachine::from_checkpoint_for_attempt(checkpoint, replacement_attempt_id).unwrap();
    let restored_event = restored
        .record_restored("checkpoint-digest")
        .expect("a live checkpoint can resume on a replacement attempt");

    assert_eq!(restored_event.attempt_id, replacement_attempt_id);
    assert_eq!(restored_event.sequence, 2);
    assert_eq!(restored_event.event_type, "run.restored");
    assert_eq!(
        restored_event.payload["checkpoint_digest"],
        "checkpoint-digest"
    );
}
