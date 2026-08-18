use agent_model_gateway_protocol::v1::model_event;
use agent_model_gateway_protocol::v1::model_execution_server::{
    ModelExecution, ModelExecutionServer,
};
use agent_model_gateway_protocol::v1::{
    Completed, FinishReason, ModelEvent, ModelInvocation, TextDelta, ToolCall as WireToolCall,
    Usage,
};
use agent_protocol::{
    ApprovalMode, EventEnvelope, ModelFinishReason, ModelStreamEvent, Placement,
    RUN_CANCELLATION_SCHEMA_VERSION, RunCancellationCommand, RunCheckpointPublished,
    RunExecutionAccepted, RunExecutionCommand, RunRecoveryCommand, RunSteeringCommand,
    RunSteeringOutcome, RunSteeringRequest, RunSteeringTarget, SandboxClass,
    TOOL_APPROVAL_DECISION_SCHEMA_VERSION, ToolApprovalDecision, ToolApprovalDecisionCommand,
    ToolDescriptor, ToolEffect, WorkerHeartbeat, WorkloadIdentityRenewalCommand,
};
use agent_runtime_worker::{
    CHECKPOINT_SUBJECT, CheckpointPayloadStore, CheckpointStoreContext, CheckpointStoreError,
    EXECUTION_ACCEPTED_SUBJECT, NatsWorker, RUN_EVENT_SUBJECT, RUN_STEERING_OUTCOME_SUBJECT,
    WORKER_EVENT_STREAM_NAME, WORKER_HEARTBEAT_SUBJECT, WorkerPollResult, WorkerProcessor,
    WorkerToolDefinition, approval_subject, cancellation_subject, execution_subject,
    identity_renewal_subject, recovery_subject, steering_subject,
};
use agent_tool_runtime::{
    ToolExecutionContext, ToolExecutionError, ToolExecutionResult, ToolExecutor,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::future::BoxFuture;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::{Stream, wrappers::TcpListenerStream};
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use uuid::Uuid;

const EXECUTION_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v1.example.json");
const EXECUTION_V2_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v2.example.json");

fn transport_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Clone)]
struct FailingCheckpointStore(CheckpointStoreError);

impl CheckpointPayloadStore for FailingCheckpointStore {
    fn put<'a>(
        &'a self,
        _context: &'a CheckpointStoreContext,
        _payload_ref: &'a str,
        _payload: &'a [u8],
    ) -> BoxFuture<'a, Result<(), CheckpointStoreError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }

    fn get<'a>(
        &'a self,
        _context: &'a CheckpointStoreContext,
        _payload_ref: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, CheckpointStoreError>> {
        Box::pin(async move { Err(self.0.clone()) })
    }
}

#[derive(Clone)]
struct RecoveringCheckpointStore {
    get_attempts: Arc<AtomicUsize>,
    payload: Arc<Vec<u8>>,
}

impl CheckpointPayloadStore for RecoveringCheckpointStore {
    fn put<'a>(
        &'a self,
        _context: &'a CheckpointStoreContext,
        _payload_ref: &'a str,
        _payload: &'a [u8],
    ) -> BoxFuture<'a, Result<(), CheckpointStoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn get<'a>(
        &'a self,
        _context: &'a CheckpointStoreContext,
        _payload_ref: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, CheckpointStoreError>> {
        Box::pin(async move {
            if self.get_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(CheckpointStoreError::Unavailable(
                    "injected gateway outage".into(),
                ))
            } else {
                Ok(self.payload.as_ref().clone())
            }
        })
    }
}

fn external_recovery_command(replacement_worker_id: Uuid) -> RunRecoveryCommand {
    external_recovery_fixture(replacement_worker_id).0
}

fn external_recovery_fixture(replacement_worker_id: Uuid) -> (RunRecoveryCommand, Vec<u8>) {
    let mut source: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    source.worker_id = Uuid::now_v7();
    source.worker_incarnation_id = source.worker_id;
    source.issued_at = chrono::Utc::now();
    source.lease_expires_at = source.issued_at + chrono::Duration::seconds(30);
    let mut large_input = String::with_capacity(900 * 1024);
    for index in 0_u64..14_400 {
        large_input.push_str(&hex::encode(Sha256::digest(index.to_le_bytes())));
    }
    source.input = large_input;
    let mut source_processor = WorkerProcessor::new(
        source.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    source_processor
        .accept(source.clone(), chrono::Utc::now())
        .unwrap();
    source_processor.start(source.attempt_id).unwrap();
    let snapshot = source_processor.checkpoint(source.attempt_id).unwrap();
    let mut execution = source.clone();
    execution.message_id = Uuid::now_v7();
    execution.attempt_id = Uuid::now_v7();
    execution.worker_id = replacement_worker_id;
    execution.worker_incarnation_id = replacement_worker_id;
    execution.owner_epoch += 1;
    execution.fencing_token = Uuid::now_v7();
    execution.issued_at = chrono::Utc::now();
    execution.lease_expires_at = execution.issued_at + chrono::Duration::seconds(30);

    let prepared = RunCheckpointPublished::prepare_v2(
        &snapshot,
        source.owner_epoch,
        source.fencing_token,
        hex::encode(Sha256::digest(b"{}")),
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(prepared.message.payload_ref.is_some());

    let payload = prepared.external_payload.unwrap();
    (
        RunRecoveryCommand {
            schema_version: 1,
            message_id: Uuid::now_v7(),
            execution,
            checkpoint: prepared.message,
            subagent_result: None,
            steering: None,
        },
        payload,
    )
}

#[derive(Debug)]
struct SuccessfulToolExecutor;

impl ToolExecutor for SuccessfulToolExecutor {
    fn implementation_digest(&self) -> &str {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }

    fn execute(
        &self,
        request: agent_protocol::ToolExecutionRequest,
        _context: ToolExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>
    {
        Box::pin(async move {
            Ok(ToolExecutionResult {
                content: serde_json::json!({
                    "path": request.call.arguments["path"],
                    "content": "workspace contents"
                }),
                is_error: false,
                exit_code: 0,
            })
        })
    }
}

fn tool_workspace_root(command: &RunExecutionCommand) -> PathBuf {
    let root = std::env::temp_dir().join(format!("worker-tools-{}", Uuid::now_v7()));
    std::fs::create_dir_all(
        root.join(command.tenant_id.to_string())
            .join(command.workspace_id.to_string()),
    )
    .unwrap();
    root
}

#[test]
fn worker_incarnations_have_disjoint_targeted_command_subjects() {
    let worker_id = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();

    assert_ne!(
        execution_subject(worker_id, first),
        execution_subject(worker_id, second)
    );
    assert_ne!(
        cancellation_subject(worker_id, first),
        cancellation_subject(worker_id, second)
    );
    assert_ne!(
        approval_subject(worker_id, first),
        approval_subject(worker_id, second)
    );
    assert_ne!(
        recovery_subject(worker_id, first),
        recovery_subject(worker_id, second)
    );
    assert_ne!(
        identity_renewal_subject(worker_id, first),
        identity_renewal_subject(worker_id, second)
    );
}

#[tokio::test]
async fn worker_consumes_a_signed_identity_renewal_on_its_incarnation_subject() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.worker_incarnation_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let signing_key = SigningKey::from_bytes(&[37; 32]);
    let processor = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    worker.set_workload_token_verifier(WorkloadTokenVerifier::new(signing_key.verifying_key()));
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_incarnation_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    let issued_at = chrono::Utc::now();
    let renewal = signed_identity_renewal(
        &command,
        issued_at,
        issued_at + chrono::Duration::seconds(30),
        &signing_key,
    );
    jetstream
        .publish(
            identity_renewal_subject(command.worker_id, command.worker_incarnation_id),
            serde_json::to_vec(&renewal).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_identity_renewal_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::IdentityRenewed
    );
}

fn signed_identity_renewal(
    command: &RunExecutionCommand,
    issued_at: chrono::DateTime<chrono::Utc>,
    lease_expires_at: chrono::DateTime<chrono::Utc>,
    signing_key: &SigningKey,
) -> WorkloadIdentityRenewalCommand {
    let claims = WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: command.tenant_id,
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: command.run_id,
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_incarnation_id,
        model_policy_id: command.model_policy_id,
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["checkpoint-gateway".into(), "model-gateway".into()]),
        scopes: BTreeSet::from([
            "checkpoint.read".into(),
            "checkpoint.write".into(),
            "model.execute".into(),
        ]),
        issued_at_unix_ms: issued_at.timestamp_millis(),
        expires_at_unix_ms: lease_expires_at.timestamp_millis(),
    };
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let input = format!("v2.{encoded}");
    let token = format!(
        "{input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(signing_key.sign(input.as_bytes()).to_bytes())
    );
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "message_id": Uuid::now_v7(),
        "tenant_id": command.tenant_id,
        "run_id": command.run_id,
        "attempt_id": command.attempt_id,
        "worker_id": command.worker_id,
        "worker_incarnation_id": command.worker_incarnation_id,
        "owner_epoch": command.owner_epoch,
        "fencing_token": command.fencing_token,
        "generation": 2,
        "issued_at": issued_at,
        "lease_expires_at": lease_expires_at,
        "workload_token": token,
    }))
    .unwrap()
}

#[tokio::test]
async fn worker_publishes_heartbeat_then_accepts_and_acks_targeted_execution() {
    let _guard = transport_lock().lock().await;
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    command.validate().expect("fresh command must validate");
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("worker config");
    let mut worker = NatsWorker::connect(&nats_url, processor)
        .await
        .expect("worker must connect");
    let observer = async_nats::connect(&nats_url)
        .await
        .expect("observer must connect");
    let jetstream = async_nats::jetstream::new(observer.clone());
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .expect("worker event stream");

    worker.publish_heartbeat().await.expect("heartbeat publish");
    let heartbeat_message = worker_events
        .get_last_raw_message_by_subject(WORKER_HEARTBEAT_SUBJECT)
        .await
        .expect("persisted heartbeat");
    let heartbeat: WorkerHeartbeat =
        serde_json::from_slice(&heartbeat_message.payload).expect("heartbeat payload");
    assert_eq!(heartbeat.worker_id, command.worker_id);
    assert_eq!(heartbeat.active_runs, 0);

    let subject = execution_subject(command.worker_id, command.worker_id);
    jetstream
        .publish(subject, serde_json::to_vec(&command).unwrap().into())
        .await
        .expect("execution publish")
        .await
        .expect("execution PubAck");

    assert_eq!(
        worker
            .poll_once(Duration::from_secs(2))
            .await
            .expect("worker poll"),
        WorkerPollResult::Accepted
    );
    let acceptance_message = worker_events
        .get_last_raw_message_by_subject(EXECUTION_ACCEPTED_SUBJECT)
        .await
        .expect("persisted acceptance");
    let acceptance: RunExecutionAccepted =
        serde_json::from_slice(&acceptance_message.payload).expect("acceptance payload");
    assert_eq!(acceptance.run_id, command.run_id);
    assert_eq!(acceptance.attempt_id, command.attempt_id);

    let run_event_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .expect("persisted kernel event");
    let run_event: EventEnvelope =
        serde_json::from_slice(&run_event_message.payload).expect("run event payload");
    assert_eq!(run_event.run_id, command.run_id);
    assert_eq!(run_event.attempt_id, command.attempt_id);
    assert_eq!(run_event.sequence, 1);
    assert_eq!(run_event.event_type, "run.started");
}

#[tokio::test]
async fn draining_worker_publishes_closed_admission_and_naks_racing_new_work() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let draining_since = chrono::Utc::now();
    let drain_deadline = draining_since + chrono::Duration::seconds(30);

    worker
        .begin_draining(draining_since, drain_deadline)
        .unwrap();
    worker.publish_heartbeat().await.unwrap();

    let heartbeat_message = worker_events
        .get_last_raw_message_by_subject(WORKER_HEARTBEAT_SUBJECT)
        .await
        .unwrap();
    let heartbeat: WorkerHeartbeat = serde_json::from_slice(&heartbeat_message.payload).unwrap();
    assert!(!heartbeat.accepting_work);
    assert_eq!(heartbeat.draining_since, Some(draining_since));
    assert_eq!(heartbeat.drain_deadline, Some(drain_deadline));

    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::RetryScheduled
    );
}

#[tokio::test]
async fn worker_publishes_a_durable_checkpoint_and_restores_it_on_a_fenced_replacement() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut original_command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    original_command.worker_id = Uuid::now_v7();
    original_command.issued_at = chrono::Utc::now();
    original_command.lease_expires_at = original_command.issued_at + chrono::Duration::seconds(30);
    let original_processor = WorkerProcessor::new(
        original_command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut original = NatsWorker::connect(&nats_url, original_processor)
        .await
        .unwrap();
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            execution_subject(original_command.worker_id, original_command.worker_id),
            serde_json::to_vec(&original_command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        original.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let checkpoint_message = worker_events
        .get_last_raw_message_by_subject(CHECKPOINT_SUBJECT)
        .await
        .expect("run.started must be followed by a persisted checkpoint");
    let checkpoint: RunCheckpointPublished =
        serde_json::from_slice(&checkpoint_message.payload).unwrap();
    checkpoint.validate().unwrap();
    assert_eq!(checkpoint.sequence, 1);
    assert_eq!(checkpoint.attempt_id, original_command.attempt_id);

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = original_command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = chrono::Utc::now();
    replacement_command.lease_expires_at =
        replacement_command.issued_at + chrono::Duration::seconds(30);
    let recovery = RunRecoveryCommand {
        schema_version: 1,
        message_id: Uuid::now_v7(),
        execution: replacement_command.clone(),
        checkpoint,
        subagent_result: None,
        steering: None,
    };
    recovery.validate().unwrap();
    let replacement_processor = WorkerProcessor::new(
        replacement_worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut replacement = NatsWorker::connect(&nats_url, replacement_processor)
        .await
        .unwrap();
    jetstream
        .publish(
            recovery_subject(replacement_worker_id, replacement_worker_id),
            serde_json::to_vec(&recovery).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        replacement
            .poll_recovery_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Restored
    );
    let restored_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let restored: EventEnvelope = serde_json::from_slice(&restored_message.payload).unwrap();
    assert_eq!(restored.event_type, "run.restored");
    assert_eq!(restored.sequence, 2);
    assert_eq!(restored.attempt_id, replacement_command.attempt_id);
}

#[tokio::test]
async fn missing_external_checkpoint_naks_recovery_for_delayed_retry() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let worker_id = Uuid::now_v7();
    let recovery = external_recovery_command(worker_id);
    recovery.validate().unwrap();
    let processor =
        WorkerProcessor::new(worker_id, vec![Placement::Cloud], 1, "0.1.0".to_string()).unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    worker.set_checkpoint_store(Arc::new(FailingCheckpointStore(
        CheckpointStoreError::NotFound,
    )));
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            recovery_subject(worker_id, worker_id),
            serde_json::to_vec(&recovery).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_recovery_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::RetryScheduled
    );
}

#[tokio::test]
async fn checkpoint_gateway_outage_redelivers_and_restores_after_the_store_recovers() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let worker_id = Uuid::now_v7();
    let (recovery, payload) = external_recovery_fixture(worker_id);
    recovery.validate().unwrap();
    let processor =
        WorkerProcessor::new(worker_id, vec![Placement::Cloud], 1, "0.1.0".to_string()).unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    let get_attempts = Arc::new(AtomicUsize::new(0));
    worker.set_checkpoint_store(Arc::new(RecoveringCheckpointStore {
        get_attempts: get_attempts.clone(),
        payload: Arc::new(payload),
    }));
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            recovery_subject(worker_id, worker_id),
            serde_json::to_vec(&recovery).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_recovery_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::RetryScheduled
    );
    assert_eq!(
        worker
            .poll_recovery_once(Duration::from_secs(3))
            .await
            .unwrap(),
        WorkerPollResult::Restored
    );
    assert_eq!(get_attempts.load(Ordering::SeqCst), 2);
    let restored_message = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap()
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let restored: EventEnvelope = serde_json::from_slice(&restored_message.payload).unwrap();
    assert_eq!(restored.event_type, "run.restored");
    assert_eq!(restored.attempt_id, recovery.execution.attempt_id);
}

#[tokio::test]
async fn recovery_checkpoints_a_pending_steer_on_the_replacement_attempt_before_ack() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let worker_id = Uuid::now_v7();
    let (mut recovery, payload) = external_recovery_fixture(worker_id);
    let issued_at = chrono::Utc::now();
    let steering = RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: recovery.execution.tenant_id,
            run_id: recovery.execution.run_id,
            attempt_id: recovery.execution.attempt_id,
            worker_id: recovery.execution.worker_id,
            worker_incarnation_id: recovery.execution.worker_incarnation_id,
        },
        RunSteeringRequest {
            input: "Resume with the newly supplied direction.".into(),
            issued_at,
            expires_at: issued_at + chrono::Duration::seconds(30),
        },
    );
    recovery.schema_version = 3;
    recovery.steering = Some(steering.clone());
    recovery.validate().unwrap();
    let processor =
        WorkerProcessor::new(worker_id, vec![Placement::Cloud], 1, "0.1.0".to_string()).unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    worker.set_checkpoint_store(Arc::new(RecoveringCheckpointStore {
        get_attempts: Arc::new(AtomicUsize::new(1)),
        payload: Arc::new(payload),
    }));
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            recovery_subject(worker_id, worker_id),
            serde_json::to_vec(&recovery).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_recovery_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Restored
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let applied: EventEnvelope = serde_json::from_slice(
        &worker_events
            .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
            .await
            .unwrap()
            .payload,
    )
    .unwrap();
    let checkpoint: RunCheckpointPublished = serde_json::from_slice(
        &worker_events
            .get_last_raw_message_by_subject(CHECKPOINT_SUBJECT)
            .await
            .unwrap()
            .payload,
    )
    .unwrap();
    assert_eq!(applied.event_type, "run.steer.applied");
    assert_eq!(applied.attempt_id, recovery.execution.attempt_id);
    assert_eq!(
        applied.payload["steering_id"],
        steering.steering_id.to_string()
    );
    assert_eq!(checkpoint.sequence, applied.sequence);
}

#[tokio::test]
async fn corrupt_external_checkpoint_terms_recovery_without_replay() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let worker_id = Uuid::now_v7();
    let recovery = external_recovery_command(worker_id);
    recovery.validate().unwrap();
    let processor =
        WorkerProcessor::new(worker_id, vec![Placement::Cloud], 1, "0.1.0".to_string()).unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    worker.set_checkpoint_store(Arc::new(FailingCheckpointStore(
        CheckpointStoreError::Corrupt,
    )));
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            recovery_subject(worker_id, worker_id),
            serde_json::to_vec(&recovery).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_recovery_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Terminated
    );
}

#[tokio::test]
async fn recovered_waiting_approval_is_rebound_before_the_recovery_command_is_acked() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut original_command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    original_command.worker_id = Uuid::now_v7();
    original_command.issued_at = chrono::Utc::now();
    original_command.lease_expires_at = original_command.issued_at + chrono::Duration::seconds(30);
    original_command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::RestrictedContainer,
            implementation_digest: "a".repeat(64),
            required_scopes: BTreeSet::from(["workspace:read".into()]),
        },
        description: "Read a file from the workspace".into(),
        input_schema: serde_json::json!({"type":"object"}),
    };
    let mut original = WorkerProcessor::new(
        original_command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    original.register_tool(definition.clone()).unwrap();
    original
        .accept(original_command.clone(), original_command.issued_at)
        .unwrap();
    original.start(original_command.attempt_id).unwrap();
    original
        .apply_model_event(
            original_command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_read".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"README.md"}),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            original_command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let plan = original
        .plan_next_tool_call(original_command.attempt_id)
        .unwrap();
    let agent_kernel::ToolPlan::ApprovalRequired(approval) = plan.plan else {
        panic!("ask policy must create a pending approval");
    };
    let checkpoint = original
        .checkpoint_message(original_command.attempt_id, chrono::Utc::now())
        .unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = original_command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = chrono::Utc::now();
    replacement_command.lease_expires_at =
        replacement_command.issued_at + chrono::Duration::seconds(30);
    let recovery = RunRecoveryCommand {
        schema_version: 1,
        message_id: Uuid::now_v7(),
        execution: replacement_command.clone(),
        checkpoint: checkpoint.clone(),
        subagent_result: None,
        steering: None,
    };
    let mut replacement_processor = WorkerProcessor::new(
        replacement_worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement_processor.register_tool(definition).unwrap();
    let mut replacement = NatsWorker::connect(&nats_url, replacement_processor)
        .await
        .unwrap();
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            recovery_subject(replacement_worker_id, replacement_worker_id),
            serde_json::to_vec(&recovery).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        replacement
            .poll_recovery_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Restored
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let rebound_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let rebound: EventEnvelope = serde_json::from_slice(&rebound_message.payload).unwrap();
    assert_eq!(rebound.event_type, "approval.rebound");
    assert_eq!(rebound.sequence, checkpoint.sequence + 2);
    assert_eq!(rebound.attempt_id, replacement_command.attempt_id);
    assert_eq!(
        rebound.payload["approval"]["approval_id"],
        approval.approval_id.to_string()
    );
    let persisted_checkpoint = worker_events
        .get_last_raw_message_by_subject(CHECKPOINT_SUBJECT)
        .await
        .unwrap();
    let persisted_checkpoint: RunCheckpointPublished =
        serde_json::from_slice(&persisted_checkpoint.payload).unwrap();
    assert_eq!(persisted_checkpoint.sequence, rebound.sequence);
    assert_eq!(
        persisted_checkpoint.status,
        agent_protocol::RunStatus::WaitingApproval
    );
}

#[tokio::test]
async fn worker_consumes_targeted_cancellation_and_releases_capacity_after_terminal_puback() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    let issued_at = chrono::Utc::now();
    let cancellation = RunCancellationCommand {
        schema_version: RUN_CANCELLATION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: command.tenant_id,
        run_id: command.run_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_id,
        issued_at,
        expires_at: issued_at + chrono::Duration::seconds(30),
        reason: "user_requested".into(),
    };
    jetstream
        .publish(
            cancellation_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&cancellation).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_cancellation_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Cancelled
    );
    worker.publish_heartbeat().await.unwrap();
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let heartbeat_message = worker_events
        .get_last_raw_message_by_subject(WORKER_HEARTBEAT_SUBJECT)
        .await
        .unwrap();
    let heartbeat: WorkerHeartbeat = serde_json::from_slice(&heartbeat_message.payload).unwrap();
    let run_event_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let terminal: EventEnvelope = serde_json::from_slice(&run_event_message.payload).unwrap();

    assert_eq!(terminal.event_type, "run.cancelled");
    assert_eq!(terminal.sequence, 2);
    assert_eq!(heartbeat.active_runs, 0);
}

#[tokio::test]
async fn worker_checkpoints_steering_before_acknowledging_and_continuing() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    let issued_at = chrono::Utc::now();
    let steering = RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_id,
        },
        RunSteeringRequest {
            input: "Focus on the authorization failure first.".into(),
            issued_at,
            expires_at: issued_at + chrono::Duration::seconds(30),
        },
    );
    jetstream
        .publish(
            steering_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&steering).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_steering_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Steered
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let run_event: EventEnvelope = serde_json::from_slice(
        &worker_events
            .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
            .await
            .unwrap()
            .payload,
    )
    .unwrap();
    let checkpoint: RunCheckpointPublished = serde_json::from_slice(
        &worker_events
            .get_last_raw_message_by_subject(CHECKPOINT_SUBJECT)
            .await
            .unwrap()
            .payload,
    )
    .unwrap();

    assert_eq!(run_event.event_type, "run.steer.applied");
    assert_eq!(
        run_event.payload["steering_id"],
        steering.steering_id.to_string()
    );
    assert_eq!(checkpoint.sequence, run_event.sequence);
}

#[tokio::test]
async fn expired_steering_publishes_a_bound_negative_outcome_before_terminating_delivery() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    let issued_at = chrono::Utc::now() - chrono::Duration::seconds(31);
    let steering = RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_id,
        },
        RunSteeringRequest {
            input: "This input must never enter the transcript.".into(),
            issued_at,
            expires_at: issued_at + chrono::Duration::seconds(30),
        },
    );
    jetstream
        .publish(
            steering_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&steering).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_steering_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Terminated
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let outcome: RunSteeringOutcome = serde_json::from_slice(
        &worker_events
            .get_last_raw_message_by_subject(RUN_STEERING_OUTCOME_SUBJECT)
            .await
            .unwrap()
            .payload,
    )
    .unwrap();

    assert_eq!(outcome.steering_id, steering.steering_id);
    assert_eq!(outcome.tenant_id, steering.tenant_id);
    assert_eq!(outcome.run_id, steering.run_id);
    assert_eq!(outcome.attempt_id, steering.attempt_id);
    assert_eq!(outcome.worker_id, steering.worker_id);
    assert_eq!(
        outcome.worker_incarnation_id,
        steering.worker_incarnation_id
    );
    assert_eq!(outcome.input_digest, steering.input_digest);
    assert_eq!(outcome.outcome, "rejected");
    assert_eq!(outcome.reason, "expired");
}

#[tokio::test]
async fn worker_discards_pre_steer_model_events_and_relaunches_with_the_new_input() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let gateway = SteeringModelGateway {
        turn: Arc::new(AtomicUsize::new(0)),
        invocations: invocations.clone(),
    };
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_model_gateway_service(gateway).await;
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker =
        NatsWorker::connect_with_model_gateway(&nats_url, processor, &gateway_endpoint)
            .await
            .unwrap();
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if invocations.lock().unwrap().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let issued_at = chrono::Utc::now();
    let steering = RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_id,
        },
        RunSteeringRequest {
            input: "Focus on the authorization failure first.".into(),
            issued_at,
            expires_at: issued_at + chrono::Duration::seconds(30),
        },
    );
    jetstream
        .publish(
            steering_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&steering).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker
            .poll_steering_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Steered
    );

    for _ in 0..4 {
        assert_eq!(
            worker
                .poll_model_once(Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerPollResult::ModelExecutionFinished
        );
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if invocations.lock().unwrap().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    {
        let invocations = invocations.lock().unwrap();
        let last_message = invocations[1].messages.last().unwrap();
        assert_eq!(
            last_message.role,
            agent_model_gateway_protocol::v1::ModelRole::User as i32
        );
        let Some(agent_model_gateway_protocol::v1::content_part::Body::Text(text)) =
            last_message.content.last().unwrap().body.as_ref()
        else {
            panic!("steering input must be a text content part");
        };
        assert_eq!(text.text, "Focus on the authorization failure first.");
    }

    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let last_event: EventEnvelope = serde_json::from_slice(
        &worker_events
            .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
            .await
            .unwrap()
            .payload,
    )
    .unwrap();
    assert_eq!(last_event.event_type, "run.steer.applied");

    jetstream
        .publish(
            steering_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&steering).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker
            .poll_steering_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Steered
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelExecutionFinished
    );
    assert_eq!(invocations.lock().unwrap().len(), 2);

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn worker_defers_steering_until_an_already_applied_model_event_is_checkpointed() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let (gateway_endpoint, gateway_shutdown, gateway_server) = spawn_model_gateway().await;
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker =
        NatsWorker::connect_with_model_gateway(&nats_url, processor, &gateway_endpoint)
            .await
            .unwrap();
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    jetstream
        .delete_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    assert!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .is_err()
    );
    jetstream
        .create_stream(async_nats::jetstream::stream::Config {
            name: WORKER_EVENT_STREAM_NAME.to_string(),
            subjects: vec!["runtime.worker.>".to_string()],
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        })
        .await
        .unwrap();

    let issued_at = chrono::Utc::now();
    let steering = RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_id,
        },
        RunSteeringRequest {
            input: "Do not overtake the pending model event.".into(),
            issued_at,
            expires_at: issued_at + chrono::Duration::seconds(30),
        },
    );
    jetstream
        .publish(
            steering_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&steering).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker
            .poll_steering_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::RetryScheduled
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_steering_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::Steered
    );

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn worker_consumes_bound_approval_decision_and_publishes_resume_before_ack() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    command.delegated_scopes = BTreeSet::from(["workspace:write".into()]);
    let mut processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    processor
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "read_file".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["workspace:write".into()]),
            },
            description: "Read a file from the workspace".into(),
            input_schema: serde_json::json!({"type":"object"}),
        })
        .unwrap();
    processor
        .accept(
            command.clone(),
            command.issued_at + chrono::Duration::seconds(1),
        )
        .unwrap();
    processor.start(command.attempt_id).unwrap();
    processor
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_read".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"README.md"}),
            },
        )
        .unwrap();
    processor
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = processor.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::ApprovalRequired(approval) = planned.plan else {
        panic!("read_file must wait for approval");
    };
    let mut worker = NatsWorker::connect(&nats_url, processor).await.unwrap();
    worker.set_workspace_root(tool_workspace_root(&command));
    worker
        .register_tool_executor(
            "read_file",
            SandboxClass::RestrictedContainer,
            Arc::new(SuccessfulToolExecutor),
        )
        .unwrap();
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    let issued_at = chrono::Utc::now();
    let decision = ToolApprovalDecisionCommand {
        schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: command.tenant_id,
        run_id: command.run_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_id,
        approval_id: approval.approval_id,
        approval_version: 2,
        binding_digest: approval.execution.binding_digest.clone(),
        decision: ToolApprovalDecision::AllowOnce,
        decision_reason: None,
        issued_at,
        expires_at: issued_at + chrono::Duration::seconds(30),
    };
    jetstream
        .publish(
            approval_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&decision).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker
            .poll_approval_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::ApprovalApplied
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let started: EventEnvelope = serde_json::from_slice(&message.payload).unwrap();
    assert_eq!(started.event_type, "tool.execution.started");
    assert_eq!(started.attempt_id, command.attempt_id);
    assert_eq!(
        started.payload["execution"]["binding_digest"],
        approval.execution.binding_digest
    );

    assert_eq!(
        worker.poll_tool_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::ToolResultPublished
    );
    let message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let result: EventEnvelope = serde_json::from_slice(&message.payload).unwrap();
    assert_eq!(result.event_type, "tool.result");
    assert_eq!(result.payload["tool_call_id"], "call_read");
    assert_eq!(result.payload["content"]["content"], "workspace contents");
}

#[tokio::test]
async fn worker_drives_model_updates_to_durable_terminal_event_before_releasing_capacity() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let (gateway_endpoint, gateway_shutdown, gateway_server) = spawn_model_gateway().await;
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker =
        NatsWorker::connect_with_model_gateway(&nats_url, processor, &gateway_endpoint)
            .await
            .unwrap();
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelExecutionFinished
    );
    worker.publish_heartbeat().await.unwrap();

    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let terminal_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let terminal: EventEnvelope = serde_json::from_slice(&terminal_message.payload).unwrap();
    let heartbeat_message = worker_events
        .get_last_raw_message_by_subject(WORKER_HEARTBEAT_SUBJECT)
        .await
        .unwrap();
    let heartbeat: WorkerHeartbeat = serde_json::from_slice(&heartbeat_message.payload).unwrap();
    assert_eq!(terminal.event_type, "run.succeeded");
    assert_eq!(terminal.sequence, 4);
    assert_eq!(heartbeat.active_runs, 0);

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn worker_checkpoints_over_budget_usage_before_publishing_the_terminal_failure() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_model_gateway_service(OverBudgetModelGateway).await;
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.budget.max_tokens = 100;
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut worker =
        NatsWorker::connect_with_model_gateway(&nats_url, processor, &gateway_endpoint)
            .await
            .unwrap();
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let checkpoint_message = worker_events
        .get_last_raw_message_by_subject(CHECKPOINT_SUBJECT)
        .await
        .unwrap();
    let checkpoint: RunCheckpointPublished =
        serde_json::from_slice(&checkpoint_message.payload).unwrap();
    let snapshot = checkpoint.decode_snapshot().unwrap();
    let state: serde_json::Value = serde_json::from_slice(&snapshot.state).unwrap();
    assert_eq!(state["budget_usage"]["tokens"], 101);
    assert_eq!(state["pending_budget_exhaustion"]["dimension"], "tokens");

    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    let terminal_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let terminal: EventEnvelope = serde_json::from_slice(&terminal_message.payload).unwrap();
    assert_eq!(terminal.event_type, "run.failed");
    assert_eq!(terminal.payload["kind"], "budget_exhausted");
    assert_eq!(terminal.payload["dimension"], "tokens");
    assert_eq!(terminal.sequence, 3);

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn worker_retries_one_unauthenticated_model_call_only_after_a_new_identity_generation() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (gateway_shutdown, shutdown_rx) = oneshot::channel();
    let service = AuthenticationThenSuccessGateway {
        calls: calls.clone(),
    };
    let gateway_server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let gateway_endpoint = format!("http://{address}");
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.worker_incarnation_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let signing_key = SigningKey::from_bytes(&[53; 32]);
    let processor = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    let mut worker =
        NatsWorker::connect_with_model_gateway(&nats_url, processor, &gateway_endpoint)
            .await
            .unwrap();
    worker.set_workload_token_verifier(WorkloadTokenVerifier::new(signing_key.verifying_key()));
    let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_incarnation_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::RetryScheduled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let issued_at = chrono::Utc::now();
    let renewal = signed_identity_renewal(
        &command,
        issued_at,
        issued_at + chrono::Duration::seconds(30),
        &signing_key,
    );
    jetstream
        .publish(
            identity_renewal_subject(command.worker_id, command.worker_incarnation_id),
            serde_json::to_vec(&renewal).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        worker
            .poll_identity_renewal_once(Duration::from_secs(2))
            .await
            .unwrap(),
        WorkerPollResult::IdentityRenewed
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelExecutionFinished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn worker_automatically_closes_a_model_tool_model_loop() {
    let _guard = transport_lock().lock().await;
    let Ok(nats_url) = std::env::var("TEST_NATS_URL") else {
        eprintln!("TEST_NATS_URL is not set; external NATS integration test skipped");
        return;
    };
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let gateway = ToolLoopModelGateway {
        turn: Arc::new(AtomicUsize::new(0)),
        invocations: invocations.clone(),
    };
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_model_gateway_service(gateway).await;
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.worker_id = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let mut processor = WorkerProcessor::new(
        command.worker_id,
        vec![Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    processor
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "read_file".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Allow,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["workspace:read".into()]),
            },
            description: "Read a workspace file".into(),
            input_schema: serde_json::json!({"type":"object"}),
        })
        .unwrap();
    let mut worker =
        NatsWorker::connect_with_model_gateway(&nats_url, processor, &gateway_endpoint)
            .await
            .unwrap();
    worker.set_workspace_root(tool_workspace_root(&command));
    worker
        .register_tool_executor(
            "read_file",
            SandboxClass::RestrictedContainer,
            Arc::new(SuccessfulToolExecutor),
        )
        .unwrap();
    let observer = async_nats::connect(&nats_url).await.unwrap();
    let jetstream = async_nats::jetstream::new(observer);
    jetstream
        .publish(
            execution_subject(command.worker_id, command.worker_id),
            serde_json::to_vec(&command).unwrap().into(),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    assert_eq!(
        worker.poll_once(Duration::from_secs(2)).await.unwrap(),
        WorkerPollResult::Accepted
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ToolExecutionStarted
    );
    let worker_events = jetstream
        .get_stream(WORKER_EVENT_STREAM_NAME)
        .await
        .unwrap();
    let started_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let started: EventEnvelope = serde_json::from_slice(&started_message.payload).unwrap();
    assert_eq!(started.event_type, "tool.execution.started");
    assert_eq!(started.payload["execution"]["call"]["id"], "call_read");
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelExecutionFinished
    );
    assert_eq!(
        worker.poll_tool_once(Duration::from_secs(1)).await.unwrap(),
        WorkerPollResult::ToolResultPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelEventPublished
    );
    assert_eq!(
        worker
            .poll_model_once(Duration::from_secs(1))
            .await
            .unwrap(),
        WorkerPollResult::ModelExecutionFinished
    );

    let terminal_message = worker_events
        .get_last_raw_message_by_subject(RUN_EVENT_SUBJECT)
        .await
        .unwrap();
    let terminal: EventEnvelope = serde_json::from_slice(&terminal_message.payload).unwrap();
    assert_eq!(terminal.event_type, "run.succeeded");
    {
        let invocations = invocations.lock().unwrap();
        assert_eq!(invocations.len(), 2);
        assert_eq!(
            invocations[1].messages.last().unwrap().role,
            agent_model_gateway_protocol::v1::ModelRole::Tool as i32
        );
    }

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[derive(Clone, Copy)]
struct SuccessfulModelGateway;

#[derive(Clone, Copy)]
struct OverBudgetModelGateway;

#[tonic::async_trait]
impl ModelExecution for OverBudgetModelGateway {
    type ExecuteStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send>>;

    async fn execute(
        &self,
        _request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(
            ModelEvent {
                schema_version: 1,
                sequence: 1,
                body: Some(model_event::Body::Usage(Usage {
                    input_tokens: 80,
                    output_tokens: 21,
                    cost_micros: 1,
                })),
            },
        )]))))
    }
}

#[derive(Clone)]
struct AuthenticationThenSuccessGateway {
    calls: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl ModelExecution for AuthenticationThenSuccessGateway {
    type ExecuteStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send>>;

    async fn execute(
        &self,
        _request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Status::unauthenticated("workload identity expired"));
        }
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(
            ModelEvent {
                schema_version: 1,
                sequence: 1,
                body: Some(model_event::Body::Completed(Completed {
                    reason: FinishReason::Stop as i32,
                })),
            },
        )]))))
    }
}

#[tonic::async_trait]
impl ModelExecution for SuccessfulModelGateway {
    type ExecuteStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send>>;

    async fn execute(
        &self,
        _request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let events = vec![
            Ok(ModelEvent {
                schema_version: 1,
                sequence: 1,
                body: Some(model_event::Body::TextDelta(TextDelta {
                    text: "hello".into(),
                    block: None,
                })),
            }),
            Ok(ModelEvent {
                schema_version: 1,
                sequence: 2,
                body: Some(model_event::Body::Usage(Usage {
                    input_tokens: 4,
                    output_tokens: 1,
                    cost_micros: 6,
                })),
            }),
            Ok(ModelEvent {
                schema_version: 1,
                sequence: 3,
                body: Some(model_event::Body::Completed(Completed {
                    reason: FinishReason::Stop as i32,
                })),
            }),
        ];
        Ok(Response::new(Box::pin(tokio_stream::iter(events))))
    }
}

#[derive(Clone)]
struct ToolLoopModelGateway {
    turn: Arc<AtomicUsize>,
    invocations: Arc<Mutex<Vec<ModelInvocation>>>,
}

#[derive(Clone)]
struct SteeringModelGateway {
    turn: Arc<AtomicUsize>,
    invocations: Arc<Mutex<Vec<ModelInvocation>>>,
}

#[tonic::async_trait]
impl ModelExecution for SteeringModelGateway {
    type ExecuteStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send>>;

    async fn execute(
        &self,
        request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        self.invocations.lock().unwrap().push(request.into_inner());
        let events = if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 1,
                    body: Some(model_event::Body::TextDelta(TextDelta {
                        text: "stale".into(),
                        block: None,
                    })),
                }),
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 2,
                    body: Some(model_event::Body::Usage(Usage {
                        input_tokens: 4,
                        output_tokens: 1,
                        cost_micros: 6,
                    })),
                }),
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 3,
                    body: Some(model_event::Body::Completed(Completed {
                        reason: FinishReason::Stop as i32,
                    })),
                }),
            ]
        } else {
            vec![Ok(ModelEvent {
                schema_version: 1,
                sequence: 1,
                body: Some(model_event::Body::Completed(Completed {
                    reason: FinishReason::Stop as i32,
                })),
            })]
        };
        Ok(Response::new(Box::pin(tokio_stream::iter(events))))
    }
}

#[tonic::async_trait]
impl ModelExecution for ToolLoopModelGateway {
    type ExecuteStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send>>;

    async fn execute(
        &self,
        request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        self.invocations.lock().unwrap().push(request.into_inner());
        let events = if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 1,
                    body: Some(model_event::Body::ToolCall(WireToolCall {
                        id: "call_read".into(),
                        name: "read_file".into(),
                        arguments_json: serde_json::to_vec(
                            &serde_json::json!({"path":"README.md"}),
                        )
                        .unwrap(),
                    })),
                }),
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 2,
                    body: Some(model_event::Body::Completed(Completed {
                        reason: FinishReason::ToolCalls as i32,
                    })),
                }),
            ]
        } else {
            vec![
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 1,
                    body: Some(model_event::Body::TextDelta(TextDelta {
                        text: "done".into(),
                        block: None,
                    })),
                }),
                Ok(ModelEvent {
                    schema_version: 1,
                    sequence: 2,
                    body: Some(model_event::Body::Completed(Completed {
                        reason: FinishReason::Stop as i32,
                    })),
                }),
            ]
        };
        Ok(Response::new(Box::pin(tokio_stream::iter(events))))
    }
}

async fn spawn_model_gateway() -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    spawn_model_gateway_service(SuccessfulModelGateway).await
}

async fn spawn_model_gateway_service<T>(
    service: T,
) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>)
where
    T: ModelExecution + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    (format!("http://{address}"), shutdown_tx, server)
}
