mod support;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use agent_model_gateway::{
    ModelPolicyRouteResolver, ProviderExecutionError, execute_with_safe_failover,
};
use agent_protocol::{ContentPart, Message, ModelRequest, ReasoningPolicy, Role};
use base64::Engine;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::{OsRng, RngCore};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use support::spawn_http_server;

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentPart::Text {
                text: "hello".into(),
            }],
        }],
        tools: vec![],
        output_schema: None,
        reasoning: ReasoningPolicy::Minimal,
        max_output_tokens: 64,
    }
}

fn encrypted_snapshot(
    tenant_id: Uuid,
    provider_id: Uuid,
    endpoint: String,
    plaintext: &str,
) -> (String, Vec<u8>) {
    let mut rng = OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 3072).unwrap();
    let public_key = RsaPublicKey::from(&private_key);
    let public_der = public_key.to_public_key_der().unwrap();
    let key_id = hex::encode(Sha256::digest(public_der.as_ref()));
    let mut data_key = [0_u8; 32];
    let mut nonce = [0_u8; 12];
    rng.fill_bytes(&mut data_key);
    rng.fill_bytes(&mut nonce);
    let encrypted_key = public_key
        .encrypt(&mut rng, Oaep::new::<Sha256>(), &data_key)
        .unwrap();
    let cipher = Aes256Gcm::new_from_slice(&data_key).unwrap();
    let aad = format!("{tenant_id}:{provider_id}");
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    let snapshot = serde_json::to_vec(&json!({
        "schema_version": 1,
        "routing": "ordered_failover",
        "candidates": [{
            "provider_id": provider_id,
            "protocol": "openai_compatible",
            "endpoint": endpoint,
            "model": "test-model",
            "credential_envelope": {
                "schema_version": 1,
                "key_id": key_id,
                "algorithm": "RSA-OAEP-256+A256GCM",
                "encrypted_key": base64::engine::general_purpose::STANDARD.encode(encrypted_key),
                "nonce": base64::engine::general_purpose::STANDARD.encode(nonce),
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(ciphertext)
            }
        }]
    }))
    .unwrap();
    (
        private_key
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string(),
        snapshot,
    )
}

#[tokio::test]
async fn gateway_resolves_and_uses_a_tenant_bound_encrypted_credential() {
    let response = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) =
        spawn_http_server("/v1/chat/completions", 200, response).await;
    let tenant_id = Uuid::now_v7();
    let provider_id = Uuid::now_v7();
    let (private_key, snapshot) =
        encrypted_snapshot(tenant_id, provider_id, endpoint, "tenant-provider-secret");
    let resolver = ModelPolicyRouteResolver::from_pkcs8_pem(
        &private_key,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();

    let routes = resolver.resolve(tenant_id, &snapshot).unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);
    let selected =
        execute_with_safe_failover(&routes, &request(), CancellationToken::new(), events_tx)
            .await
            .unwrap();

    assert_eq!(selected.provider_id, provider_id.to_string());
    assert!(
        captured
            .await
            .unwrap()
            .head
            .contains("tenant-provider-secret")
    );
    assert!(!format!("{routes:?}").contains("tenant-provider-secret"));
    server.await.unwrap();
}

#[test]
fn credential_envelope_cannot_be_replayed_for_another_tenant() {
    let tenant_id = Uuid::now_v7();
    let provider_id = Uuid::now_v7();
    let (private_key, snapshot) = encrypted_snapshot(
        tenant_id,
        provider_id,
        "https://models.example.test/v1/chat/completions".into(),
        "tenant-provider-secret",
    );
    let resolver = ModelPolicyRouteResolver::from_pkcs8_pem(
        &private_key,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();

    assert!(matches!(
        resolver.resolve(Uuid::now_v7(), &snapshot),
        Err(ProviderExecutionError::InvalidConfiguration(message))
            if message == "provider credential envelope could not be opened"
    ));
}
