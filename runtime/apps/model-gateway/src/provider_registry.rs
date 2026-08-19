use crate::{
    AnthropicMessagesAdapter, AnthropicMessagesConfig, OpenAiCompatibleAdapter,
    OpenAiCompatibleConfig, OpenAiResponsesAdapter, OpenAiResponsesConfig, ProviderAdapter,
    ProviderCredential, ProviderExecutionError, ProviderPricing, ProviderProtocol, ProviderRoute,
};
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;
use zeroize::Zeroizing;

const ENVELOPE_ALGORITHM: &str = "RSA-OAEP-256+A256GCM";
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct ModelPolicyRouteResolver {
    private_key: RsaPrivateKey,
    key_id: String,
    response_timeout: Duration,
    stream_idle_timeout: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPolicySnapshot {
    schema_version: u32,
    routing: String,
    candidates: Vec<ProviderSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSnapshot {
    provider_id: Uuid,
    protocol: String,
    endpoint: String,
    model: String,
    credential_envelope: CredentialEnvelope,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialEnvelope {
    schema_version: u32,
    key_id: String,
    algorithm: String,
    encrypted_key: String,
    nonce: String,
    ciphertext: String,
}

impl ModelPolicyRouteResolver {
    pub fn from_pkcs8_pem(
        pem: &str,
        response_timeout: Duration,
        stream_idle_timeout: Duration,
    ) -> Result<Self, ProviderExecutionError> {
        let private_key = RsaPrivateKey::from_pkcs8_pem(pem).map_err(|_| {
            ProviderExecutionError::InvalidConfiguration(
                "model credential private key is not valid PKCS#8 PEM".into(),
            )
        })?;
        if private_key.n().bits() < 3072
            || response_timeout.is_zero()
            || stream_idle_timeout.is_zero()
        {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "model credential key must be RSA-3072 or stronger and timeouts must be positive"
                    .into(),
            ));
        }
        let public_der = RsaPublicKey::from(&private_key)
            .to_public_key_der()
            .map_err(|_| {
                ProviderExecutionError::InvalidConfiguration(
                    "model credential public key could not be encoded".into(),
                )
            })?;
        Ok(Self {
            private_key,
            key_id: hex::encode(Sha256::digest(public_der.as_ref())),
            response_timeout,
            stream_idle_timeout,
        })
    }

    pub fn resolve(
        &self,
        tenant_id: Uuid,
        snapshot_json: &[u8],
    ) -> Result<Vec<ProviderRoute>, ProviderExecutionError> {
        if tenant_id.is_nil()
            || snapshot_json.is_empty()
            || snapshot_json.len() > MAX_SNAPSHOT_BYTES
        {
            return Err(invalid_snapshot());
        }
        let snapshot: ModelPolicySnapshot =
            serde_json::from_slice(snapshot_json).map_err(|_| invalid_snapshot())?;
        let candidate_count = snapshot.candidates.len();
        let valid_routing = match snapshot.routing.as_str() {
            "single_provider" => candidate_count == 1,
            "ordered_failover" => (1..=8).contains(&candidate_count),
            _ => false,
        };
        if snapshot.schema_version != 1 || !valid_routing {
            return Err(invalid_snapshot());
        }
        let mut provider_ids = BTreeSet::new();
        let mut routes = Vec::with_capacity(candidate_count);
        for candidate in snapshot.candidates {
            if candidate.provider_id.is_nil() || !provider_ids.insert(candidate.provider_id) {
                return Err(invalid_snapshot());
            }
            let credential = self.open_credential(
                tenant_id,
                candidate.provider_id,
                &candidate.credential_envelope,
            )?;
            let protocol = candidate.protocol.parse::<ProviderProtocol>()?;
            let pricing = ProviderPricing {
                input_million_tokens_micros: 0,
                output_million_tokens_micros: 0,
            };
            let adapter = match protocol {
                ProviderProtocol::OpenAiCompatible => {
                    ProviderAdapter::from(OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
                        endpoint: candidate.endpoint,
                        model: candidate.model,
                        pricing,
                        response_timeout: self.response_timeout,
                        stream_idle_timeout: self.stream_idle_timeout,
                        // Not carried by the routing config yet, so the field
                        // is not sent and the provider applies its own default.
                        // That is the whole fix for the blocker: what was being
                        // sent was the Run's budget, which no real model
                        // accepts. Letting an operator set a real per-reply
                        // ceiling for their model is the follow-up, and it
                        // belongs on the routing candidate beside the model.
                        max_output_tokens: None,
                    })?)
                }
                ProviderProtocol::OpenAiResponses => {
                    ProviderAdapter::from(OpenAiResponsesAdapter::new(OpenAiResponsesConfig {
                        endpoint: candidate.endpoint,
                        model: candidate.model,
                        pricing,
                        response_timeout: self.response_timeout,
                        stream_idle_timeout: self.stream_idle_timeout,
                    })?)
                }
                ProviderProtocol::AnthropicMessages => {
                    ProviderAdapter::from(AnthropicMessagesAdapter::new(AnthropicMessagesConfig {
                        endpoint: candidate.endpoint,
                        model: candidate.model,
                        anthropic_version: "2023-06-01".into(),
                        pricing,
                        response_timeout: self.response_timeout,
                        stream_idle_timeout: self.stream_idle_timeout,
                    })?)
                }
            };
            routes.push(ProviderRoute::new(
                candidate.provider_id.to_string(),
                adapter,
                credential,
            ));
        }
        Ok(routes)
    }

    fn open_credential(
        &self,
        tenant_id: Uuid,
        provider_id: Uuid,
        envelope: &CredentialEnvelope,
    ) -> Result<ProviderCredential, ProviderExecutionError> {
        if envelope.schema_version != 1
            || envelope.algorithm != ENVELOPE_ALGORITHM
            || envelope.key_id != self.key_id
        {
            return Err(open_failed());
        }
        let decode = |value: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(value)
                .map_err(|_| open_failed())
        };
        let encrypted_key = decode(&envelope.encrypted_key)?;
        let nonce = decode(&envelope.nonce)?;
        let ciphertext = decode(&envelope.ciphertext)?;
        if nonce.len() != 12 || ciphertext.is_empty() {
            return Err(open_failed());
        }
        let data_key = Zeroizing::new(
            self.private_key
                .decrypt(Oaep::new::<Sha256>(), &encrypted_key)
                .map_err(|_| open_failed())?,
        );
        if data_key.len() != 32 {
            return Err(open_failed());
        }
        let cipher = Aes256Gcm::new_from_slice(data_key.as_slice()).map_err(|_| open_failed())?;
        let aad = format!("{tenant_id}:{provider_id}");
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| open_failed())?,
        );
        let credential = std::str::from_utf8(plaintext.as_slice()).map_err(|_| open_failed())?;
        ProviderCredential::bearer(credential.to_owned()).map_err(|_| open_failed())
    }
}

fn invalid_snapshot() -> ProviderExecutionError {
    ProviderExecutionError::InvalidConfiguration("model policy snapshot is invalid".into())
}

fn open_failed() -> ProviderExecutionError {
    ProviderExecutionError::InvalidConfiguration(
        "provider credential envelope could not be opened".into(),
    )
}
