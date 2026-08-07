use agent_protocol::{Placement, RunPriority, RunQueuedCommand};

const EXAMPLE: &str = include_str!("../../../../contracts/events/run-queued.v1.example.json");

#[test]
fn run_queued_v1_example_decodes_and_validates() {
    let command: RunQueuedCommand = serde_json::from_str(EXAMPLE).expect("example must decode");

    assert_eq!(command.schema_version, 1);
    assert_eq!(command.priority, RunPriority::Interactive);
    assert_eq!(command.placement, Placement::Cloud);
    assert_eq!(command.budget.max_tokens, 12_000);
    assert!(command.validate().is_ok());
}

#[test]
fn run_queued_rejects_unknown_schema_version() {
    let mut command: RunQueuedCommand = serde_json::from_str(EXAMPLE).expect("example must decode");
    command.schema_version = 2;

    assert_eq!(
        command
            .validate()
            .expect_err("unknown version must fail")
            .to_string(),
        "unsupported run queued schema version 2"
    );
}

#[test]
fn run_queued_rejects_empty_work() {
    let mut command: RunQueuedCommand = serde_json::from_str(EXAMPLE).expect("example must decode");
    command.input = "   ".to_string();

    let error = command.validate().expect_err("invalid work must fail");

    assert_eq!(error.to_string(), "run input must not be blank");
}

#[test]
fn run_queued_rejects_zero_token_budget() {
    let mut command: RunQueuedCommand = serde_json::from_str(EXAMPLE).expect("example must decode");
    command.budget.max_tokens = 0;

    let error = command.validate().expect_err("unbounded work must fail");

    assert_eq!(error.to_string(), "run budgets must be finite and positive");
}

#[test]
fn run_queued_rejects_execution_longer_than_one_day() {
    let mut command: RunQueuedCommand = serde_json::from_str(EXAMPLE).expect("example must decode");
    command.budget.max_duration_seconds = 86_401;

    let error = command.validate().expect_err("overlong work must fail");

    assert_eq!(
        error.to_string(),
        "run duration must not exceed 86400 seconds"
    );
}
