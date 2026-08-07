use agent_grpc_security::{ClientMtlsMaterials, ServerMtlsMaterials};

#[test]
fn blank_or_incomplete_mtls_material_is_rejected_before_startup() {
    assert!(ServerMtlsMaterials::new(Vec::new(), b"key".to_vec(), b"ca".to_vec()).is_err());
    assert!(
        ClientMtlsMaterials::new(
            b"cert".to_vec(),
            b"key".to_vec(),
            b"ca".to_vec(),
            " ".into(),
        )
        .is_err()
    );
}
