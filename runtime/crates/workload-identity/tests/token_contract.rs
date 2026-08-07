use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenError,
    WorkloadTokenVerifier,
};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use uuid::Uuid;

#[test]
fn v2_token_requires_capability_and_exact_worker_incarnation_binding() {
    let signing_key = SigningKey::from_bytes(&[7; 32]);
    let claims = claims();
    let token = sign_v2(&signing_key, &claims);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    let verified = verifier
        .verify(
            &token,
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
            1_785_542_410_000,
        )
        .unwrap();

    assert!(verified.authorizes(&WorkloadIdentityBinding {
        tenant_id: claims.tenant_id,
        run_id: claims.run_id,
        attempt_id: claims.attempt_id,
        worker_id: claims.worker_id,
        worker_incarnation_id: claims.worker_incarnation_id,
    }));
    assert!(!verified.authorizes(&WorkloadIdentityBinding {
        worker_incarnation_id: Uuid::now_v7(),
        ..WorkloadIdentityBinding::from(&claims)
    }));
}

#[test]
fn verifier_can_be_constructed_from_the_control_plane_base64_public_key() {
    let signing_key = SigningKey::from_bytes(&[41; 32]);
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(signing_key.verifying_key().to_bytes());

    assert!(WorkloadTokenVerifier::from_base64(&encoded).is_ok());
    assert!(WorkloadTokenVerifier::from_base64("not-a-public-key").is_err());
}

#[test]
fn token_for_model_execution_cannot_be_reused_for_checkpoint_write() {
    let signing_key = SigningKey::from_bytes(&[8; 32]);
    let mut claims = claims();
    claims.scopes = BTreeSet::from(["model.execute".to_string()]);
    let token = sign_v2(&signing_key, &claims);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    assert_eq!(
        verifier.verify(
            &token,
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
            1_785_542_410_000,
        ),
        Err(WorkloadTokenError::MissingCapability)
    );
}

#[test]
fn v3_token_requires_a_well_formed_model_policy_digest() {
    let signing_key = SigningKey::from_bytes(&[9; 32]);
    let mut claims = claims();
    claims.schema_version = 3;
    claims.model_policy_digest = "a".repeat(64);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    assert!(
        verifier
            .verify(
                &sign_v2(&signing_key, &claims),
                RequiredCapability::new("model-gateway", "model.execute", true),
                1_785_542_410_000,
            )
            .is_ok()
    );

    claims.model_policy_digest = "tampered".into();
    assert_eq!(
        verifier.verify(
            &sign_v2(&signing_key, &claims),
            RequiredCapability::new("model-gateway", "model.execute", true),
            1_785_542_410_000,
        ),
        Err(WorkloadTokenError::InvalidClaims)
    );
}

fn claims() -> WorkloadIdentityClaims {
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
        audiences: BTreeSet::from([
            "model-gateway".to_string(),
            "checkpoint-gateway".to_string(),
        ]),
        scopes: BTreeSet::from([
            "model.execute".to_string(),
            "checkpoint.read".to_string(),
            "checkpoint.write".to_string(),
        ]),
        issued_at_unix_ms: 1_785_542_400_000,
        expires_at_unix_ms: 1_785_542_430_000,
    }
}

fn sign_v2(signing_key: &SigningKey, claims: &WorkloadIdentityClaims) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = signing_key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}
