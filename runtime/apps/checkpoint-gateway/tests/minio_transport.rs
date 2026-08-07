use agent_checkpoint_gateway::{S3CheckpointStoreConfig, build_s3_checkpoint_store};
use object_store::ObjectStoreExt;
use object_store::path::Path;
use uuid::Uuid;

#[tokio::test]
async fn minio_adapter_round_trips_checkpoint_bytes() {
    let Ok(endpoint) = std::env::var("TEST_MINIO_ENDPOINT") else {
        eprintln!("TEST_MINIO_ENDPOINT is not set; real MinIO test skipped");
        return;
    };
    let store = build_s3_checkpoint_store(S3CheckpointStoreConfig {
        endpoint,
        bucket: std::env::var("TEST_MINIO_BUCKET")
            .unwrap_or_else(|_| "agent-runtime-checkpoints".into()),
        region: "us-east-1".into(),
        access_key_id: std::env::var("TEST_MINIO_ACCESS_KEY")
            .unwrap_or_else(|_| "local-minio-admin".into()),
        secret_access_key: std::env::var("TEST_MINIO_SECRET_KEY")
            .unwrap_or_else(|_| "local-minio-password".into()),
        allow_http: true,
    })
    .unwrap();
    let location = Path::parse(format!("integration/{}.zst", Uuid::now_v7())).unwrap();
    let expected = b"real minio checkpoint".to_vec();

    store.put(&location, expected.clone().into()).await.unwrap();
    let actual = store.get(&location).await.unwrap().bytes().await.unwrap();
    store.delete(&location).await.unwrap();

    assert_eq!(actual.as_ref(), expected);
}
