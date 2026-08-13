use crate::wire::edge_node_session_client::EdgeNodeSessionClient;
use crate::wire::{
    ClientHello, NodeToControl, OutboxBatch, SessionProof, control_to_node, node_to_control,
};
use crate::{EdgeDeviceIdentity, EdgeNode, EdgeNodeError, EdgeOutboxRecord};
use agent_grpc_security::ClientMtlsMaterials;
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Endpoint;
use uuid::Uuid;

const CLIENT_FRAME_SCHEMA_VERSION: u32 = 1;
const MAX_SESSION_NONCE_BYTES: usize = 64;
const MAX_OUTBOX_BATCH_BYTES: usize = 3 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct EdgeOutboundConfig {
    endpoint: String,
    tls: ClientMtlsMaterials,
    connect_timeout: Duration,
    stream_idle_timeout: Duration,
    outbox_batch_limit: usize,
    reconnect_initial_delay: Duration,
    reconnect_max_delay: Duration,
}

impl EdgeOutboundConfig {
    pub fn new(endpoint: String, tls: ClientMtlsMaterials) -> Result<Self, EdgeNodeError> {
        if !endpoint.starts_with("https://") {
            return Err(EdgeNodeError::InvalidOutboundConfiguration);
        }
        Ok(Self {
            endpoint,
            tls,
            connect_timeout: Duration::from_secs(10),
            stream_idle_timeout: Duration::from_secs(30),
            outbox_batch_limit: 256,
            reconnect_initial_delay: Duration::from_millis(250),
            reconnect_max_delay: Duration::from_secs(30),
        })
    }

    pub fn with_reconnect_delays(
        mut self,
        initial_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self, EdgeNodeError> {
        if initial_delay.is_zero()
            || max_delay < initial_delay
            || max_delay > Duration::from_secs(5 * 60)
        {
            return Err(EdgeNodeError::InvalidOutboundConfiguration);
        }
        self.reconnect_initial_delay = initial_delay;
        self.reconnect_max_delay = max_delay;
        Ok(self)
    }
}

pub struct EdgeOutboundConnector {
    identity: EdgeDeviceIdentity,
    node: Arc<EdgeNode>,
    config: EdgeOutboundConfig,
}

impl EdgeOutboundConnector {
    #[must_use]
    pub fn new(
        identity: EdgeDeviceIdentity,
        node: Arc<EdgeNode>,
        config: EdgeOutboundConfig,
    ) -> Self {
        Self {
            identity,
            node,
            config,
        }
    }

    pub async fn connect_once(&self) -> Result<(), EdgeNodeError> {
        let endpoint = Endpoint::from_shared(self.config.endpoint.clone())
            .map_err(|error| EdgeNodeError::Transport(error.to_string()))?
            .connect_timeout(self.config.connect_timeout)
            .tls_config(self.config.tls.clone().into_tonic())
            .map_err(|error| EdgeNodeError::Transport(error.to_string()))?;
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| EdgeNodeError::Transport(error.to_string()))?;
        let (tx, rx) = mpsc::channel(32);
        tx.send(NodeToControl {
            frame: Some(node_to_control::Frame::Hello(self.client_hello())),
        })
        .await
        .map_err(|_| EdgeNodeError::Transport("outbound stream closed before hello".into()))?;
        let mut inbound = EdgeNodeSessionClient::new(channel)
            .open_session(ReceiverStream::new(rx))
            .await
            .map_err(|error| EdgeNodeError::Transport(error.to_string()))?
            .into_inner();
        let mut challenged_session = None;
        let mut accepted_session = None;
        let mut pending_batch: Option<(String, u64)> = None;

        loop {
            let frame = tokio::time::timeout(self.config.stream_idle_timeout, inbound.message())
                .await
                .map_err(|_| EdgeNodeError::Transport("control stream idle timeout".into()))?
                .map_err(|error| EdgeNodeError::Transport(error.to_string()))?;
            let Some(frame) = frame else {
                return Ok(());
            };
            match frame.frame.ok_or(EdgeNodeError::InvalidControlFrame)? {
                control_to_node::Frame::Challenge(challenge) => {
                    if challenged_session.is_some()
                        || challenge.schema_version != CLIENT_FRAME_SCHEMA_VERSION
                        || challenge.nonce.len() < 16
                        || challenge.nonce.len() > MAX_SESSION_NONCE_BYTES
                    {
                        return Err(EdgeNodeError::InvalidControlFrame);
                    }
                    let session_id = Uuid::parse_str(&challenge.session_id)
                        .map_err(|_| EdgeNodeError::InvalidControlFrame)?;
                    let now = chrono::Utc::now().timestamp_millis();
                    let proof_expiry = challenge.expires_at_unix_ms.min(now + 60_000);
                    let proof = self.identity.create_session_proof(
                        session_id,
                        &challenge.nonce,
                        &self.node.enrollment,
                        now,
                        proof_expiry,
                    )?;
                    tx.send(NodeToControl {
                        frame: Some(node_to_control::Frame::Proof(SessionProof {
                            schema_version: CLIENT_FRAME_SCHEMA_VERSION,
                            proof_token: proof,
                        })),
                    })
                    .await
                    .map_err(|_| EdgeNodeError::Transport("control stream closed".into()))?;
                    challenged_session = Some(session_id);
                }
                control_to_node::Frame::Accepted(accepted) => {
                    let session_id = Uuid::parse_str(&accepted.session_id)
                        .map_err(|_| EdgeNodeError::InvalidControlFrame)?;
                    if accepted.schema_version != CLIENT_FRAME_SCHEMA_VERSION
                        || challenged_session != Some(session_id)
                        || accepted_session.is_some()
                    {
                        return Err(EdgeNodeError::InvalidControlFrame);
                    }
                    accepted_session = Some(session_id);
                    self.send_pending_batch(&tx, session_id, &mut pending_batch)
                        .await?;
                }
                control_to_node::Frame::Task(task) => {
                    let session_id = accepted_session.ok_or(EdgeNodeError::InvalidControlFrame)?;
                    if task.schema_version != CLIENT_FRAME_SCHEMA_VERSION
                        || task.task_token.is_empty()
                    {
                        return Err(EdgeNodeError::InvalidControlFrame);
                    }
                    self.node
                        .execute_task_token(&task.task_token, chrono::Utc::now().timestamp_millis())
                        .await?;
                    self.send_pending_batch(&tx, session_id, &mut pending_batch)
                        .await?;
                }
                control_to_node::Frame::Ack(ack) => {
                    let session_id = accepted_session.ok_or(EdgeNodeError::InvalidControlFrame)?;
                    if ack.schema_version != CLIENT_FRAME_SCHEMA_VERSION {
                        return Err(EdgeNodeError::InvalidControlFrame);
                    }
                    let (batch_digest, _) = pending_batch
                        .take()
                        .ok_or(EdgeNodeError::InvalidControlFrame)?;
                    self.node.apply_signed_outbox_ack(
                        &ack.ack_token,
                        session_id,
                        &batch_digest,
                        chrono::Utc::now().timestamp_millis(),
                    )?;
                    self.send_pending_batch(&tx, session_id, &mut pending_batch)
                        .await?;
                }
                control_to_node::Frame::Revoked(revoked) => {
                    if accepted_session.is_none()
                        || revoked.schema_version != CLIENT_FRAME_SCHEMA_VERSION
                        || revoked.revocation_token.is_empty()
                    {
                        return Err(EdgeNodeError::InvalidControlFrame);
                    }
                    self.node.apply_signed_enrollment_revocation(
                        &revoked.revocation_token,
                        chrono::Utc::now().timestamp_millis(),
                    )?;
                    return Err(EdgeNodeError::EnrollmentRevoked);
                }
            }
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), EdgeNodeError> {
        let mut delay = self.config.reconnect_initial_delay;
        loop {
            let result = tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                result = self.connect_once() => result,
            };
            if let Err(error) = result
                && terminal_connection_error(&error)
            {
                return Err(error);
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(delay) => {}
            }
            delay = delay
                .checked_mul(2)
                .unwrap_or(self.config.reconnect_max_delay)
                .min(self.config.reconnect_max_delay);
        }
    }

    fn client_hello(&self) -> ClientHello {
        let enrollment = self.node.enrollment.claims();
        ClientHello {
            schema_version: CLIENT_FRAME_SCHEMA_VERSION,
            device_id: self.identity.device_id().to_string(),
            enrollment_id: enrollment.enrollment_id.to_string(),
            node_id: enrollment.node_id.to_string(),
            node_generation: enrollment.node_generation,
            enrollment_grant_digest: self.node.enrollment.grant_digest().into(),
            capability_manifest_digest: enrollment.capability_manifest_digest.clone(),
            approved_capabilities: enrollment.approved_capabilities.iter().cloned().collect(),
        }
    }

    async fn send_pending_batch(
        &self,
        tx: &mpsc::Sender<NodeToControl>,
        session_id: Uuid,
        pending_batch: &mut Option<(String, u64)>,
    ) -> Result<(), EdgeNodeError> {
        if pending_batch.is_some() {
            return Ok(());
        }
        let candidates = self
            .node
            .pending_outbox(0, self.config.outbox_batch_limit)?;
        let (records, records_json) =
            encode_bounded_outbox_records(&candidates, MAX_OUTBOX_BATCH_BYTES)?;
        let Some(first) = records.first() else {
            return Ok(());
        };
        let last = records
            .last()
            .expect("a non-empty batch always has a last record");
        let batch_digest = hex::encode(Sha256::digest(&records_json));
        tx.send(NodeToControl {
            frame: Some(node_to_control::Frame::OutboxBatch(OutboxBatch {
                schema_version: CLIENT_FRAME_SCHEMA_VERSION,
                session_id: session_id.to_string(),
                first_sequence: first.sequence,
                last_sequence: last.sequence,
                records_json,
                batch_digest: batch_digest.clone(),
            })),
        })
        .await
        .map_err(|_| EdgeNodeError::Transport("control stream closed during upload".into()))?;
        *pending_batch = Some((batch_digest, last.sequence));
        Ok(())
    }
}

fn encode_bounded_outbox_records(
    records: &[EdgeOutboxRecord],
    max_bytes: usize,
) -> Result<(Vec<EdgeOutboxRecord>, Vec<u8>), EdgeNodeError> {
    if max_bytes < 2 {
        return Err(EdgeNodeError::InvalidOutboundConfiguration);
    }
    let mut selected = Vec::new();
    let mut encoded_size = 2_usize;
    for record in records {
        let record_size = serde_json::to_vec(record)
            .map_err(|error| EdgeNodeError::Transport(error.to_string()))?
            .len();
        let separator = usize::from(!selected.is_empty());
        if encoded_size
            .checked_add(separator)
            .and_then(|value| value.checked_add(record_size))
            .is_none_or(|value| value > max_bytes)
        {
            break;
        }
        encoded_size += separator + record_size;
        selected.push(record.clone());
    }
    if selected.is_empty() && !records.is_empty() {
        return Err(EdgeNodeError::Transport(
            "one durable outbox record exceeds the transport batch limit".into(),
        ));
    }
    let encoded = serde_json::to_vec(&selected)
        .map_err(|error| EdgeNodeError::Transport(error.to_string()))?;
    debug_assert!(encoded.len() <= max_bytes);
    Ok((selected, encoded))
}

fn terminal_connection_error(error: &EdgeNodeError) -> bool {
    matches!(
        error,
        EdgeNodeError::InvalidControlFrame
            | EdgeNodeError::InvalidSessionProof
            | EdgeNodeError::WrongEnrollment
            | EdgeNodeError::WrongNode
            | EdgeNodeError::WrongNodeGeneration
            | EdgeNodeError::UnapprovedCapability
            | EdgeNodeError::InvalidOutboxAck
            | EdgeNodeError::EnrollmentRevoked
            | EdgeNodeError::EnrollmentExpired
    )
}

#[cfg(test)]
mod tests {
    use super::encode_bounded_outbox_records;
    use crate::{EdgeOutboxPayload, EdgeOutboxRecord, EdgeTaskReceipt, EdgeTaskReceiptStatus};
    use agent_protocol::{RUNTIME_INVOCATION_SCHEMA_VERSION, RuntimeInvocationContext};
    use uuid::Uuid;

    fn record(sequence: u64, output_bytes: usize) -> EdgeOutboxRecord {
        let run_id = Uuid::from_u128(u128::from(sequence) + 100);
        EdgeOutboxRecord {
            sequence,
            payload: EdgeOutboxPayload::TaskReceipt(EdgeTaskReceipt {
                schema_version: 1,
                task_id: Uuid::from_u128(u128::from(sequence)),
                task_digest: "a".repeat(64),
                enrollment_id: Uuid::from_u128(1),
                capability_manifest_digest: "b".repeat(64),
                node_id: Uuid::from_u128(2),
                node_generation: 1,
                invocation: RuntimeInvocationContext {
                    schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
                    tenant_id: Uuid::from_u128(3),
                    application_id: Uuid::from_u128(4),
                    workload_identity_id: Uuid::from_u128(5),
                    workspace_id: Uuid::from_u128(6),
                    agent_version_id: Uuid::from_u128(7),
                    model_policy_id: Uuid::from_u128(8),
                },
                run_id,
                session_id: run_id,
                workspace_owner_epoch: 1,
                status: EdgeTaskReceiptStatus::Succeeded,
                output: "x".repeat(output_bytes),
                last_runtime_sequence: 0,
            }),
        }
    }

    #[test]
    fn outbound_batch_uses_a_bounded_contiguous_prefix() {
        let records = vec![record(1, 700_000), record(2, 700_000), record(3, 700_000)];
        let (selected, encoded) =
            encode_bounded_outbox_records(&records, 1_500_000).expect("bounded batch");

        assert_eq!(selected.len(), 2);
        assert!(encoded.len() <= 1_500_000);
        assert_eq!(selected[0].sequence, 1);
        assert_eq!(selected[1].sequence, 2);
    }
}
