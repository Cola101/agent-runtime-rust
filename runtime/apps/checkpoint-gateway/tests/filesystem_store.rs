use agent_checkpoint_gateway::{build_configured_checkpoint_store, build_local_checkpoint_store};
use bytes::Bytes;
use object_store::ObjectStoreExt;
use object_store::path::Path;
use std::fs;
use uuid::Uuid;

#[tokio::test]
async fn local_checkpoint_store_keeps_content_inside_its_project_root() {
    let root = std::env::temp_dir().join(format!("agent-runtime-checkpoints-{}", Uuid::now_v7()));
    let store = build_local_checkpoint_store(&root).unwrap();
    let location = Path::from("tenants/t1/runs/r1/checkpoints/digest.zst");

    store
        .put(&location, Bytes::from_static(b"checkpoint").into())
        .await
        .unwrap();
    let restored = store.get(&location).await.unwrap().bytes().await.unwrap();

    assert_eq!(restored, Bytes::from_static(b"checkpoint"));
    assert_eq!(
        fs::read(root.join("tenants/t1/runs/r1/checkpoints/digest.zst")).unwrap(),
        b"checkpoint"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn local_checkpoint_store_rejects_a_file_as_its_root() {
    let root = std::env::temp_dir().join(format!("agent-runtime-checkpoints-{}", Uuid::now_v7()));
    fs::write(&root, b"not a directory").unwrap();

    let error = build_local_checkpoint_store(&root).unwrap_err();

    assert!(error.to_string().contains("local checkpoint root"));
    fs::remove_file(root).unwrap();
}

#[test]
fn configured_store_uses_local_mode_without_any_s3_credentials() {
    let root = std::env::temp_dir().join(format!("agent-runtime-checkpoints-{}", Uuid::now_v7()));

    let store = build_configured_checkpoint_store(Some(root.as_path()), None).unwrap();

    assert!(format!("{store:?}").contains("LocalFileSystem"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn configured_store_requires_s3_configuration_outside_local_mode() {
    let error = build_configured_checkpoint_store(None, None).unwrap_err();

    assert!(error.to_string().contains("S3 configuration is required"));
}
