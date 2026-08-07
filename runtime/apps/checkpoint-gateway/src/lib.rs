use agent_checkpoint_gateway_protocol::v1::checkpoint_storage_server::CheckpointStorage;
use agent_checkpoint_gateway_protocol::v1::checkpoint_storage_server::CheckpointStorageServer;
use agent_checkpoint_gateway_protocol::v1::{
    GetCheckpointRequest, GetCheckpointResponse, PutCheckpointRequest, PutCheckpointResponse,
    WorkloadBinding,
};
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenError,
    WorkloadTokenVerifier,
};
use chrono::Utc;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{Error as ObjectStoreError, ObjectStore, ObjectStoreExt};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::{fs, path::Path as FileSystemPath};
use tonic::{Request, Response, Status};
use uuid::Uuid;

pub const MAX_STORED_CHECKPOINT_BYTES: usize = 17 * 1024 * 1024;
const MAX_GRPC_CHECKPOINT_MESSAGE_BYTES: usize = MAX_STORED_CHECKPOINT_BYTES + 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3CheckpointStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub allow_http: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("checkpoint object store configuration is invalid: {0}")]
pub struct CheckpointStoreConfigurationError(String);

pub fn build_s3_checkpoint_store(
    config: S3CheckpointStoreConfig,
) -> Result<Arc<dyn ObjectStore>, CheckpointStoreConfigurationError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    AmazonS3Builder::new()
        .with_endpoint(config.endpoint)
        .with_bucket_name(config.bucket)
        .with_region(config.region)
        .with_access_key_id(config.access_key_id)
        .with_secret_access_key(config.secret_access_key)
        .with_allow_http(config.allow_http)
        .build()
        .map(|store| Arc::new(store) as Arc<dyn ObjectStore>)
        .map_err(|error| CheckpointStoreConfigurationError(error.to_string()))
}

pub fn build_local_checkpoint_store(
    root: &FileSystemPath,
) -> Result<Arc<dyn ObjectStore>, CheckpointStoreConfigurationError> {
    if root.exists() && !root.is_dir() {
        return Err(CheckpointStoreConfigurationError(
            "local checkpoint root must be a directory".to_owned(),
        ));
    }
    fs::create_dir_all(root).map_err(|error| {
        CheckpointStoreConfigurationError(format!(
            "could not create local checkpoint root: {error}"
        ))
    })?;
    let canonical_root = root.canonicalize().map_err(|error| {
        CheckpointStoreConfigurationError(format!(
            "could not resolve local checkpoint root: {error}"
        ))
    })?;
    LocalFileSystem::new_with_prefix(canonical_root)
        .map(|store| Arc::new(store) as Arc<dyn ObjectStore>)
        .map_err(|error| {
            CheckpointStoreConfigurationError(format!(
                "could not initialize local checkpoint root: {error}"
            ))
        })
}

pub fn build_configured_checkpoint_store(
    local_root: Option<&FileSystemPath>,
    s3_config: Option<S3CheckpointStoreConfig>,
) -> Result<Arc<dyn ObjectStore>, CheckpointStoreConfigurationError> {
    if let Some(root) = local_root {
        return build_local_checkpoint_store(root);
    }
    let config = s3_config.ok_or_else(|| {
        CheckpointStoreConfigurationError(
            "S3 configuration is required when local checkpoint mode is disabled".to_owned(),
        )
    })?;
    build_s3_checkpoint_store(config)
}

#[derive(Clone)]
pub struct CheckpointStorageGrpcService {
    store: Arc<dyn ObjectStore>,
    verifier: WorkloadTokenVerifier,
}

impl CheckpointStorageGrpcService {
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>, verifier: WorkloadTokenVerifier) -> Self {
        Self { store, verifier }
    }

    fn authenticate<T>(
        &self,
        request: &Request<T>,
        scope: &'static str,
    ) -> Result<WorkloadIdentityClaims, Status> {
        let bearer = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("missing workload bearer token"))?;
        self.verifier
            .verify(
                bearer,
                RequiredCapability::new("checkpoint-gateway", scope, true),
                Utc::now().timestamp_millis(),
            )
            .map_err(map_token_error)
    }
}

#[must_use]
pub fn checkpoint_storage_server(
    service: CheckpointStorageGrpcService,
) -> CheckpointStorageServer<CheckpointStorageGrpcService> {
    CheckpointStorageServer::new(service)
        .max_decoding_message_size(MAX_GRPC_CHECKPOINT_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_CHECKPOINT_MESSAGE_BYTES)
}

#[tonic::async_trait]
impl CheckpointStorage for CheckpointStorageGrpcService {
    async fn put_checkpoint(
        &self,
        request: Request<PutCheckpointRequest>,
    ) -> Result<Response<PutCheckpointResponse>, Status> {
        let claims = self.authenticate(&request, "checkpoint.write")?;
        let request = request.into_inner();
        let binding = authorize_request(request.schema_version, request.binding, &claims)?;
        let digest = parse_payload_ref(&request.payload_ref)?;
        if request.payload.is_empty() || request.payload.len() > MAX_STORED_CHECKPOINT_BYTES {
            return Err(Status::invalid_argument(
                "checkpoint payload size is outside the supported range",
            ));
        }
        let actual_digest = hex::encode(Sha256::digest(&request.payload));
        if actual_digest != digest {
            return Err(Status::invalid_argument(
                "checkpoint payload does not match its content address",
            ));
        }
        let location = object_location(&binding, &digest)?;
        self.store
            .put(&location, request.payload.clone().into())
            .await
            .map_err(map_store_error)?;
        Ok(Response::new(PutCheckpointResponse {
            schema_version: 1,
            payload_ref: request.payload_ref,
            stored_payload_digest: digest,
            stored_size: request.payload.len() as u64,
        }))
    }

    async fn get_checkpoint(
        &self,
        request: Request<GetCheckpointRequest>,
    ) -> Result<Response<GetCheckpointResponse>, Status> {
        let claims = self.authenticate(&request, "checkpoint.read")?;
        let request = request.into_inner();
        let binding = authorize_request(request.schema_version, request.binding, &claims)?;
        let digest = parse_payload_ref(&request.payload_ref)?;
        let location = object_location(&binding, &digest)?;
        let payload = self
            .store
            .get(&location)
            .await
            .map_err(map_store_error)?
            .bytes()
            .await
            .map_err(map_store_error)?;
        if payload.is_empty()
            || payload.len() > MAX_STORED_CHECKPOINT_BYTES
            || hex::encode(Sha256::digest(&payload)) != digest
        {
            return Err(Status::data_loss(
                "checkpoint object failed its content-address verification",
            ));
        }
        Ok(Response::new(GetCheckpointResponse {
            schema_version: 1,
            payload_ref: request.payload_ref,
            payload: payload.to_vec(),
        }))
    }
}

fn authorize_request(
    schema_version: u32,
    binding: Option<WorkloadBinding>,
    claims: &WorkloadIdentityClaims,
) -> Result<WorkloadIdentityBinding, Status> {
    let binding =
        binding.ok_or_else(|| Status::invalid_argument("workload binding is required"))?;
    let parsed = WorkloadIdentityBinding {
        tenant_id: parse_uuid(&binding.tenant_id)?,
        run_id: parse_uuid(&binding.run_id)?,
        attempt_id: parse_uuid(&binding.attempt_id)?,
        worker_id: parse_uuid(&binding.worker_id)?,
        worker_incarnation_id: parse_uuid(&binding.worker_incarnation_id)?,
    };
    if schema_version != 1 || !claims.authorizes(&parsed) {
        return Err(Status::permission_denied(
            "workload identity does not authorize this checkpoint request",
        ));
    }
    Ok(parsed)
}

fn parse_uuid(value: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(value).map_err(|_| Status::invalid_argument("workload binding is malformed"))
}

fn parse_payload_ref(payload_ref: &str) -> Result<String, Status> {
    let digest = payload_ref
        .strip_prefix("checkpoint://sha256/")
        .ok_or_else(|| Status::invalid_argument("checkpoint payload reference is malformed"))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Status::invalid_argument(
            "checkpoint payload reference is malformed",
        ));
    }
    Ok(digest.to_owned())
}

fn object_location(binding: &WorkloadIdentityBinding, digest: &str) -> Result<Path, Status> {
    Path::parse(format!(
        "tenants/{}/runs/{}/checkpoints/{digest}.zst",
        binding.tenant_id, binding.run_id
    ))
    .map_err(|_| Status::invalid_argument("checkpoint object path is malformed"))
}

fn map_token_error(error: WorkloadTokenError) -> Status {
    match error {
        WorkloadTokenError::MissingCapability => {
            Status::permission_denied("workload token lacks checkpoint capability")
        }
        _ => Status::unauthenticated("invalid workload token"),
    }
}

fn map_store_error(error: ObjectStoreError) -> Status {
    match error {
        ObjectStoreError::NotFound { .. } => Status::not_found("checkpoint object was not found"),
        _ => Status::unavailable("checkpoint object store is unavailable"),
    }
}
