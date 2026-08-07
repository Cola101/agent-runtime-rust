use crate::{CheckpointPayloadStore, CheckpointStoreContext, CheckpointStoreError};
use agent_checkpoint_gateway_protocol::v1::checkpoint_storage_client::CheckpointStorageClient;
use agent_checkpoint_gateway_protocol::v1::{
    GetCheckpointRequest, PutCheckpointRequest, WorkloadBinding,
};
use agent_grpc_security::ClientMtlsMaterials;
use futures_util::future::BoxFuture;
use sha2::{Digest, Sha256};
use tonic::Code;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};

const MAX_GRPC_CHECKPOINT_MESSAGE_BYTES: usize = 17 * 1024 * 1024 + 64 * 1024;

#[derive(Clone)]
pub struct GrpcCheckpointPayloadStore {
    client: CheckpointStorageClient<Channel>,
}

impl GrpcCheckpointPayloadStore {
    pub async fn connect(endpoint: String) -> Result<Self, CheckpointStoreError> {
        let client = CheckpointStorageClient::connect(endpoint)
            .await
            .map_err(|error| CheckpointStoreError::Unavailable(error.to_string()))?
            .max_decoding_message_size(MAX_GRPC_CHECKPOINT_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_CHECKPOINT_MESSAGE_BYTES);
        Ok(Self { client })
    }

    pub async fn connect_with_mtls(
        endpoint: String,
        materials: ClientMtlsMaterials,
    ) -> Result<Self, CheckpointStoreError> {
        let endpoint = Endpoint::from_shared(endpoint)
            .map_err(|error| CheckpointStoreError::Unavailable(error.to_string()))?
            .tls_config(materials.into_tonic())
            .map_err(|error| CheckpointStoreError::Unavailable(error.to_string()))?;
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| CheckpointStoreError::Unavailable(error.to_string()))?;
        let client = CheckpointStorageClient::new(channel)
            .max_decoding_message_size(MAX_GRPC_CHECKPOINT_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_CHECKPOINT_MESSAGE_BYTES);
        Ok(Self { client })
    }
}

impl CheckpointPayloadStore for GrpcCheckpointPayloadStore {
    fn put<'a>(
        &'a self,
        context: &'a CheckpointStoreContext,
        payload_ref: &'a str,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<(), CheckpointStoreError>> {
        Box::pin(async move {
            let mut request = Request::new(PutCheckpointRequest {
                schema_version: 1,
                binding: Some(binding(context)),
                payload_ref: payload_ref.to_owned(),
                payload: payload.to_vec(),
            });
            authorize(&mut request, &context.workload_token)?;
            let response = self
                .client
                .clone()
                .put_checkpoint(request)
                .await
                .map_err(map_status)?
                .into_inner();
            let digest = hex::encode(Sha256::digest(payload));
            if response.schema_version != 1
                || response.payload_ref != payload_ref
                || response.stored_payload_digest != digest
                || response.stored_size != payload.len() as u64
            {
                return Err(CheckpointStoreError::Corrupt);
            }
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        context: &'a CheckpointStoreContext,
        payload_ref: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, CheckpointStoreError>> {
        Box::pin(async move {
            let mut request = Request::new(GetCheckpointRequest {
                schema_version: 1,
                binding: Some(binding(context)),
                payload_ref: payload_ref.to_owned(),
            });
            authorize(&mut request, &context.workload_token)?;
            let response = self
                .client
                .clone()
                .get_checkpoint(request)
                .await
                .map_err(map_status)?
                .into_inner();
            if response.schema_version != 1 || response.payload_ref != payload_ref {
                return Err(CheckpointStoreError::Corrupt);
            }
            Ok(response.payload)
        })
    }
}

fn binding(context: &CheckpointStoreContext) -> WorkloadBinding {
    WorkloadBinding {
        tenant_id: context.tenant_id.to_string(),
        run_id: context.run_id.to_string(),
        attempt_id: context.attempt_id.to_string(),
        worker_id: context.worker_id.to_string(),
        worker_incarnation_id: context.worker_incarnation_id.to_string(),
    }
}

fn authorize<T>(request: &mut Request<T>, token: &str) -> Result<(), CheckpointStoreError> {
    let authorization = MetadataValue::try_from(format!("Bearer {token}"))
        .map_err(|_| CheckpointStoreError::Unavailable("invalid workload token".into()))?;
    request
        .metadata_mut()
        .insert("authorization", authorization);
    Ok(())
}

fn map_status(status: tonic::Status) -> CheckpointStoreError {
    match status.code() {
        Code::NotFound => CheckpointStoreError::NotFound,
        Code::DataLoss => CheckpointStoreError::Corrupt,
        _ => CheckpointStoreError::Unavailable(status.to_string()),
    }
}
