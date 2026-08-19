use agent_kernel::{RunCommand, RunMachine, TransitionError};
use agent_protocol::{
    BudgetDimension, ModelErrorKind, ModelFinishReason, ModelStreamEvent, RunBudget, RunStatus,
    SubagentSpawnRequest, ToolEffect,
};
use uuid::Uuid;

fn machine() -> RunMachine {
    RunMachine::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    )
}

#[test]
fn approval_pause_can_resume_and_finish_without_losing_event_order() {
    let mut run = machine();

    let started = run.apply(RunCommand::Start).expect("queued run starts");
    let approval = run
        .apply(RunCommand::RequireApproval)
        .expect("running run pauses for approval");
    let resumed = run
        .apply(RunCommand::Approve)
        .expect("approved run resumes");
    let completed = run
        .apply(RunCommand::Complete)
        .expect("resumed run completes");

    assert_eq!(run.status(), RunStatus::Succeeded);
    assert_eq!(
        [
            started.sequence,
            approval.sequence,
            resumed.sequence,
            completed.sequence,
        ],
        [1, 2, 3, 4]
    );
    assert_eq!(approval.event_type, "approval.required");
    assert_eq!(resumed.event_type, "run.resumed");
}

#[test]
fn terminal_run_rejects_later_mutation() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();
    run.apply(RunCommand::Complete).unwrap();

    let error = run.apply(RunCommand::Cancel).unwrap_err();

    assert_eq!(error, TransitionError::TerminalState(RunStatus::Succeeded));
}

#[test]
fn budget_exhaustion_is_a_classified_terminal_failure() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let event = run
        .record_budget_exhausted(BudgetDimension::Tokens)
        .expect("budget exhaustion must end a running Run");

    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(event.event_type, "run.failed");
    assert_eq!(event.payload["kind"], "budget_exhausted");
    assert_eq!(event.payload["dimension"], "tokens");
}

#[test]
fn required_mcp_failure_can_end_a_run_before_model_start() {
    let mut run = machine();

    let event = run
        .record_required_mcp_unavailable(&["required-search".into()])
        .expect("an exhausted required dependency must end the queued Run");

    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(event.sequence, 1);
    assert_eq!(event.event_type, "run.failed");
    assert_eq!(event.payload["kind"], "required_mcp_unavailable");
    assert_eq!(event.payload["servers"][0], "required-search");
    assert_eq!(event.payload["retryable"], false);
}

#[test]
fn durable_subagent_request_suspends_the_parent_before_child_admission() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();
    let request = SubagentSpawnRequest {
        tool_call_id: "call-review".into(),
        delegation_id: Uuid::now_v7(),
        role: "reviewer".into(),
        input: "Review the migration evidence.".into(),
        budget: RunBudget {
            max_tokens: 400,
            max_cost_cents: 30,
            max_duration_seconds: 20,
        },
        binding_digest: "a".repeat(64),
        mode: agent_protocol::SubagentSpawnMode::Inline,
        conversation_history: Vec::new(),
    };

    let event = run
        .record_subagent_spawn_requested(&request)
        .expect("a running parent can suspend on one durable child request");

    assert_eq!(run.status(), RunStatus::Suspended);
    assert_eq!(event.event_type, "subagent.spawn.requested");
    assert_eq!(event.payload["status"], "suspended");
    assert_eq!(event.payload["request"]["tool_call_id"], "call-review");
    assert_eq!(event.payload["request"]["role"], "reviewer");
}

#[test]
fn steering_marks_an_auditable_boundary_without_finishing_the_run() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();
    let steering_id = Uuid::now_v7();

    let event = run
        .record_steering_applied(steering_id, &"a".repeat(64))
        .expect("a running Run accepts one durable steering boundary");

    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(event.event_type, "run.steer.applied");
    assert_eq!(event.payload["steering_id"], steering_id.to_string());
    assert_eq!(event.payload["input_digest"], "a".repeat(64));
}

#[test]
fn unknown_non_idempotent_tool_result_becomes_indeterminate() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let event = run
        .apply(RunCommand::ToolOutcomeUnknown {
            effect: ToolEffect::NonIdempotent,
        })
        .expect("ambiguous external side effect has an explicit terminal state");

    assert_eq!(run.status(), RunStatus::Indeterminate);
    assert_eq!(event.event_type, "run.indeterminate");
    assert_eq!(event.payload["effect"], "non_idempotent");
    assert_eq!(event.payload["replay_safe"], false);
}

#[test]
fn idempotent_tool_result_can_be_retried_without_ending_run() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let event = run
        .apply(RunCommand::ToolOutcomeUnknown {
            effect: ToolEffect::Idempotent,
        })
        .expect("idempotent tool can be retried");

    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(event.event_type, "tool.retry_requested");
}

#[test]
fn model_stream_events_are_ordered_and_completion_is_the_only_terminal_event() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let delta = run
        .apply_model_event(ModelStreamEvent::TextDelta {
            text: "hello".into(),
            block: None,
        })
        .expect("running model stream accepts deltas");
    let usage = run
        .apply_model_event(ModelStreamEvent::Usage {
            input_tokens: 12,
            output_tokens: 3,
            cost_micros: 42,
        })
        .expect("running model stream accepts usage");
    let completed = run
        .apply_model_event(ModelStreamEvent::Completed {
            reason: ModelFinishReason::Stop,
        })
        .expect("model completion ends the run");

    assert_eq!(
        [delta.sequence, usage.sequence, completed.sequence],
        [2, 3, 4]
    );
    assert_eq!(delta.event_type, "model.output.delta");
    assert_eq!(usage.event_type, "model.usage");
    assert_eq!(completed.event_type, "run.succeeded");
    assert_eq!(run.status(), RunStatus::Succeeded);
    assert_eq!(
        run.apply_model_event(ModelStreamEvent::Completed {
            reason: ModelFinishReason::Stop,
        }),
        Err(TransitionError::TerminalState(RunStatus::Succeeded))
    );
}

#[test]
fn model_turn_that_requests_tools_does_not_finish_the_run() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let turn = run
        .apply_model_event(ModelStreamEvent::Completed {
            reason: ModelFinishReason::ToolCalls,
        })
        .expect("tool request completes only the current model turn");

    assert_eq!(turn.event_type, "model.turn.completed");
    assert_eq!(turn.payload["reason"], "tool_calls");
    assert_eq!(run.status(), RunStatus::Running);
}

#[test]
fn timeout_failure_preserves_classification_and_uses_timed_out_terminal_state() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let failed = run
        .apply_model_event(ModelStreamEvent::Failed {
            kind: ModelErrorKind::Timeout,
            retryable: true,
            message: "provider stream stalled".into(),
        })
        .expect("exhausted timeout ends the run explicitly");

    assert_eq!(failed.event_type, "run.timed_out");
    assert_eq!(failed.payload["kind"], "timeout");
    assert_eq!(failed.payload["retryable"], true);
    assert_eq!(run.status(), RunStatus::TimedOut);
}

/// Which block text came from reaches the durable event, and its absence is a
/// different fact from block zero.
///
/// Without this the log cannot tell two things the model said from one thing
/// cut in half, and every client has to guess by adjacency. A provider that
/// supplies no block index still produces a usable event -- the payload simply
/// does not claim one, which is also what every record written before this
/// field existed looks like.
#[test]
fn a_text_delta_carries_the_block_it_came_from() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let first = run
        .apply_model_event(ModelStreamEvent::TextDelta {
            text: "one".into(),
            block: Some(0),
        })
        .expect("a running stream accepts deltas");
    assert_eq!(first.payload["block"], 0);
    assert_eq!(first.payload["text"], "one");

    let second = run
        .apply_model_event(ModelStreamEvent::TextDelta {
            text: "two".into(),
            block: Some(1),
        })
        .expect("a running stream accepts deltas");
    assert_eq!(second.payload["block"], 1);

    let unblocked = run
        .apply_model_event(ModelStreamEvent::TextDelta {
            text: "three".into(),
            block: None,
        })
        .expect("a running stream accepts deltas");
    // Absent, not zero. Zero is a block; nothing is the absence of one, and a
    // client that read a default here would group unrelated text together.
    assert!(unblocked.payload.get("block").is_none());
}

/// A reply that ran into its own length cap is a short answer, not a lost one.
///
/// Measured against a real self-hosted vLLM: a turn produced 6,877 reasoning
/// deltas and 1,300 output deltas, hit `max_tokens`, and the Run ended
/// `run.failed { kind: "context_overflow", reason: "length" }`. Two things
/// were wrong with that.
///
/// It was not a context overflow. The input was 4,945 tokens against a 204,800
/// window; what ran out was the *per-reply* ceiling. The two have opposite
/// fixes -- one means the conversation must be shortened, the other means the
/// ceiling must be raised or less must be asked for -- and reporting the first
/// when the second happened sends a person to shorten a conversation that is
/// nowhere near the limit.
///
/// And 1,300 deltas of real answer were on screen when the Run declared
/// itself failed. openclaw keeps that text: its
/// `packages/agent-core/src/agent-loop.test.ts:507-536` asserts the truncated
/// assistant message is emitted *and replayed into the next context*, with
/// tool calls stripped (a call cut off mid-arguments is not a call anybody
/// should run) and `execute` never invoked. We agree about keeping the text.
/// We do not follow it all the way: openclaw's loop then continues on its own,
/// and continuing costs money the person did not agree to spend, so this stops
/// and says the answer is short.
///
/// `model.turn.completed { reason: "length" }` is the shape the client is
/// already built for -- `cutShort()` in `desktop/shell/src/surfaces/model.ts`
/// has said "这段回答没说完 —— 模型到了单轮长度上限" since it was written, and
/// could never fire because the Run failed instead.
#[test]
fn a_reply_that_hit_its_length_cap_is_kept_and_called_short() {
    let mut run = machine();
    run.apply(RunCommand::Start).unwrap();

    let turn = run
        .apply_model_event(ModelStreamEvent::Completed {
            reason: ModelFinishReason::Length,
        })
        .expect("a truncated reply still completes its turn");

    assert_eq!(
        turn.event_type, "model.turn.completed",
        "a length-capped reply is a completed turn, not a failed Run: {:?}",
        turn.payload,
    );
    assert_eq!(turn.payload["reason"], "length");
    assert_ne!(
        turn.payload["kind"], "context_overflow",
        "the input fit; it was the reply that ran out of room",
    );

    // And it has to *end*. Only `ToolCalls` plans another turn
    // (`worker/src/lib.rs:10089-10116`); every other completion that leaves
    // the Run running leaves it running forever, with the composer disabled
    // and nothing on its way. Keeping the text is worth nothing if the
    // conversation can never be continued afterwards.
    assert!(
        run.status().is_terminal(),
        "a truncated reply must still end the Run, or nothing will: {:?}",
        run.status(),
    );
    assert_eq!(run.status(), RunStatus::Succeeded);
}
