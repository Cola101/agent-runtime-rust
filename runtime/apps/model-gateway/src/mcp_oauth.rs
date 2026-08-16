//! Credential-domain-owned MCP OAuth lifecycle (ADR-0118).
//!
//! Stable binding identifiers may cross the Gateway boundary. Access tokens,
//! refresh tokens, authorization codes, PKCE verifiers and the store master key
//! never implement Debug or Serialize on a public type.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::StreamExt as _;
use rand_core::{OsRng, RngCore as _};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use zeroize::{Zeroize as _, Zeroizing};

const STORE_SCHEMA_VERSION: u32 = 1;
const SEALED_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 128 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_SECRET_BYTES: usize = 16 * 1024;
const MAX_FIELD_BYTES: usize = 4 * 1024;
const MAX_SCOPES: usize = 32;
const REFRESH_SKEW_MS: i64 = 30_000;
const AUTHORIZATION_FLOW_TTL_MINUTES: i64 = 10;

#[derive(Clone, Eq, PartialEq)]
pub struct McpOAuthBinding {
    pub tenant_id: Uuid,
    pub server_id: Uuid,
    pub credential_id: Uuid,
    pub endpoint: String,
}

impl McpOAuthBinding {
    fn aad(&self) -> String {
        format!(
            "agent-runtime-mcp-oauth-v1:{}:{}:{}:{}",
            self.tenant_id, self.server_id, self.credential_id, self.endpoint
        )
    }

    fn validate(&self, loopback_permitted: bool) -> Result<(), McpOAuthError> {
        if self.tenant_id.is_nil()
            || self.server_id.is_nil()
            || self.credential_id.is_nil()
            || self.endpoint.len() > 2_048
        {
            return Err(McpOAuthError::InvalidBinding);
        }
        crate::mcp::require_permitted_endpoint(&self.endpoint, loopback_permitted)
            .map_err(|_| McpOAuthError::InvalidBinding)
    }
}

#[derive(Clone)]
pub struct McpOAuthAuthorizationRequest {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    /// Frozen with the rest of the flow. Resolving this at revoke time instead
    /// would let a server compromised after authorization name any URL and be
    /// handed the refresh token we are trying to invalidate.
    pub revocation_endpoint: Option<String>,
}

pub struct McpOAuthAuthorizationStart {
    pub flow_id: Uuid,
    pub authorization_url: String,
    pub expires_at: DateTime<Utc>,
}

/// The trusted, pre-configured public client. Dynamic Client Registration and
/// private client secrets are deliberately out of scope (ADR-0119).
#[derive(Clone)]
pub struct McpOAuthClientConfig {
    pub client_id: String,
    pub redirect_uri: String,
    pub requested_scopes: Vec<String>,
}

/// A frozen discovery result.
///
/// Every field has already passed the same outbound address policy as the MCP
/// endpoint itself. Once this is folded into a pending authorization it is
/// authoritative: the callback reads the persisted endpoints and never
/// re-resolves metadata, so a server that changes its documents mid-flow cannot
/// move where an authorization code is redeemed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpOAuthDiscovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub scopes_supported: Vec<String>,
    pub resource: String,
    /// Frozen here rather than resolved at revoke time. Re-discovering it later
    /// would let a server that has since been compromised name any URL and
    /// receive the refresh token we are trying to invalidate.
    pub revocation_endpoint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpOAuthCredentialStatus {
    Absent,
    PendingAuthorization {
        expires_at: DateTime<Utc>,
        revision: u64,
    },
    Active {
        expires_at: Option<DateTime<Utc>>,
        revision: u64,
    },
    AuthorizationRequired {
        reason: McpOAuthAuthorizationReason,
        revision: u64,
    },
    Revoked {
        revision: u64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOAuthAuthorizationReason {
    Missing,
    FlowExpired,
    ExchangeIndeterminate,
    RefreshIndeterminate,
    ProviderRejected,
    AccessTokenRejected,
    NoRefreshToken,
    Revoked,
}

/// Outcome of a revocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpOAuthRevocation {
    pub revision: u64,
    /// Whether the authorization server confirmed. Local revocation has already
    /// committed either way, so `false` means the provider could not be told --
    /// never that the credential is still usable here.
    pub remote_confirmed: bool,
}

/// Everything the remote revocation call needs, captured before the stored
/// record is replaced. Secret-bearing and deliberately not Debug.
struct RemoteRevocation {
    endpoint: String,
    client_id: String,
    is_refresh: bool,
    token: Zeroizing<String>,
}

/// Secret-bearing and credential-domain local. Deliberately not Debug/Clone.
pub struct ResolvedMcpOAuthCredential {
    access_token: Zeroizing<String>,
    token_digest: String,
    revision: u64,
}

impl ResolvedMcpOAuthCredential {
    #[must_use]
    pub fn token_digest(&self) -> &str {
        &self.token_digest
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn into_access_token(self) -> Zeroizing<String> {
        self.access_token
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpOAuthError {
    #[error("MCP OAuth binding is invalid")]
    InvalidBinding,
    #[error("MCP OAuth authorization request is invalid")]
    InvalidAuthorizationRequest,
    #[error("MCP OAuth authorization callback is invalid or stale")]
    InvalidAuthorizationCallback,
    #[error("MCP OAuth authorization is required")]
    AuthorizationRequired,
    #[error("MCP OAuth credential store is unavailable")]
    StoreUnavailable,
    #[error("MCP OAuth provider request failed")]
    ProviderUnavailable,
    #[error("MCP OAuth provider rejected the credential")]
    ProviderRejected,
    /// Discovery produced a document that failed a binding, consistency or
    /// capacity rule. The public error stays deliberately coarse so a probe
    /// cannot use it to map which check tripped.
    #[error("MCP OAuth discovery was rejected")]
    DiscoveryRejected,
}

#[derive(Clone)]
pub struct McpOAuthCoordinator {
    store: Arc<EncryptedFileCredentialStore>,
    request_timeout: Duration,
    loopback_permitted: bool,
}

impl McpOAuthCoordinator {
    pub fn new(
        state_root: impl AsRef<Path>,
        master_key: [u8; 32],
        request_timeout: Duration,
        loopback_permitted: bool,
    ) -> Result<Self, McpOAuthError> {
        if request_timeout.is_zero() {
            return Err(McpOAuthError::InvalidAuthorizationRequest);
        }
        let store = EncryptedFileCredentialStore::new(state_root.as_ref(), master_key)?;
        Ok(Self {
            store: Arc::new(store),
            request_timeout,
            loopback_permitted,
        })
    }

    /// Resolves the authorization server for an MCP endpoint.
    ///
    /// `challenge` is the raw `WWW-Authenticate` value when the endpoint sent
    /// one. A challenge may only *narrow* discovery to a metadata URL on the
    /// endpoint's own origin. Naming a foreign origin is a substitution attempt
    /// and is refused before any request leaves the process, so a hostile
    /// challenge cannot even be used to provoke an outbound probe.
    pub async fn discover(
        &self,
        binding: &McpOAuthBinding,
        challenge: Option<&str>,
    ) -> Result<McpOAuthDiscovery, McpOAuthError> {
        binding
            .validate(self.loopback_permitted)
            .map_err(|_| McpOAuthError::DiscoveryRejected)?;
        let endpoint =
            Url::parse(&binding.endpoint).map_err(|_| McpOAuthError::DiscoveryRejected)?;

        let metadata_url = match challenge.map(challenge_resource_metadata).transpose()? {
            Some(Some(named)) => {
                let named = Url::parse(&named).map_err(|_| McpOAuthError::DiscoveryRejected)?;
                if !same_origin(&endpoint, &named) {
                    return Err(McpOAuthError::DiscoveryRejected);
                }
                named
            }
            _ => well_known_url(&endpoint, "oauth-protected-resource"),
        };

        let protected: ProtectedResourceMetadata =
            self.fetch_metadata(metadata_url.as_str()).await?;
        protected.validate_capacity()?;
        // The document must claim exactly the endpoint we are authenticating to.
        if !urls_equal(&protected.resource, &binding.endpoint) {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        let issuer = protected
            .authorization_servers
            .first()
            .cloned()
            .ok_or(McpOAuthError::DiscoveryRejected)?;
        crate::mcp::require_permitted_endpoint(&issuer, self.loopback_permitted)
            .map_err(|_| McpOAuthError::DiscoveryRejected)?;
        let issuer_url = Url::parse(&issuer).map_err(|_| McpOAuthError::DiscoveryRejected)?;

        let server: AuthorizationServerMetadata = self
            .fetch_metadata(well_known_url(&issuer_url, "oauth-authorization-server").as_str())
            .await?;
        server.validate(&issuer_url, self.loopback_permitted)?;

        let scopes_supported = if protected.scopes_supported.is_empty() {
            server.scopes_supported
        } else {
            protected.scopes_supported
        };
        Ok(McpOAuthDiscovery {
            issuer: server.issuer,
            authorization_endpoint: server.authorization_endpoint,
            token_endpoint: server.token_endpoint,
            scopes_supported,
            resource: binding.endpoint.clone(),
            revocation_endpoint: server.revocation_endpoint,
        })
    }

    /// Discovers, then opens a PKCE authorization against the discovered
    /// endpoints.
    ///
    /// The endpoints are persisted into the pending record here. That is what
    /// freezes the flow: `complete_authorization` reads them back from the
    /// record and never re-resolves metadata, so a server that swaps its
    /// documents mid-flow cannot move where the code is redeemed.
    pub async fn begin_discovered_authorization(
        &self,
        binding: McpOAuthBinding,
        client: McpOAuthClientConfig,
        challenge: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<McpOAuthAuthorizationStart, McpOAuthError> {
        let discovery = self.discover(&binding, challenge).await?;
        let scopes = negotiate_scopes(&client.requested_scopes, &discovery.scopes_supported)?;
        let request = McpOAuthAuthorizationRequest {
            authorization_endpoint: discovery.authorization_endpoint,
            token_endpoint: discovery.token_endpoint,
            client_id: client.client_id,
            redirect_uri: client.redirect_uri,
            scopes,
            revocation_endpoint: discovery.revocation_endpoint,
        };
        self.begin_authorization(binding, request, now).await
    }

    async fn fetch_metadata<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<T, McpOAuthError> {
        crate::mcp::require_permitted_endpoint(url, self.loopback_permitted)
            .map_err(|_| McpOAuthError::DiscoveryRejected)?;
        let http = crate::mcp::build_pinned_http_client_for_endpoint(
            url,
            self.request_timeout,
            self.loopback_permitted,
        )
        .map_err(|_| McpOAuthError::DiscoveryRejected)?;
        let response = tokio::time::timeout(
            self.request_timeout,
            http.get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send(),
        )
        .await
        .map_err(|_| McpOAuthError::ProviderUnavailable)?
        .map_err(|_| McpOAuthError::ProviderUnavailable)?;
        if response
            .content_length()
            .is_some_and(|length| length as usize > MAX_METADATA_BYTES)
        {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        // The pinned client refuses redirects, so a 3xx arrives as a non-success
        // status rather than a silent hop to another origin.
        if !response.status().is_success() {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        let body = bounded_body(response, MAX_METADATA_BYTES).await?;
        serde_json::from_slice(&body).map_err(|_| McpOAuthError::DiscoveryRejected)
    }

    pub async fn begin_authorization(
        &self,
        binding: McpOAuthBinding,
        request: McpOAuthAuthorizationRequest,
        now: DateTime<Utc>,
    ) -> Result<McpOAuthAuthorizationStart, McpOAuthError> {
        binding.validate(self.loopback_permitted)?;
        let mut authorization_url =
            validate_authorization_request(&request, self.loopback_permitted)?;
        let mut state_bytes = Zeroizing::new([0_u8; 32]);
        let mut verifier_bytes = Zeroizing::new([0_u8; 32]);
        OsRng.fill_bytes(&mut *state_bytes);
        OsRng.fill_bytes(&mut *verifier_bytes);
        let state = Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state_bytes.as_slice()),
        );
        let verifier = Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes.as_slice()),
        );
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let flow_id = Uuid::now_v7();
        let expires_at = now + ChronoDuration::minutes(AUTHORIZATION_FLOW_TTL_MINUTES);
        authorization_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &request.client_id)
            .append_pair("redirect_uri", &request.redirect_uri)
            .append_pair("scope", &request.scopes.join(" "))
            .append_pair("state", state.as_str())
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");

        let mut lease = self.acquire(binding.clone()).await?;
        let current = lease.load()?;
        let revision = current.as_ref().map_or(1, |record| record.revision + 1);
        let record = StoredCredentialRecord {
            schema_version: STORE_SCHEMA_VERSION,
            revision,
            binding: StoredBinding::from(&binding),
            state: StoredCredentialState::PendingAuthorization {
                flow_id,
                state: state.to_string(),
                verifier: verifier.to_string(),
                authorization_endpoint: request.authorization_endpoint,
                token_endpoint: request.token_endpoint,
                client_id: request.client_id,
                redirect_uri: request.redirect_uri,
                scopes: request.scopes,
                expires_at_ms: expires_at.timestamp_millis(),
                revocation_endpoint: request.revocation_endpoint,
            },
        };
        lease.persist(current.as_ref().map(|record| record.revision), &record)?;
        Ok(McpOAuthAuthorizationStart {
            flow_id,
            authorization_url: authorization_url.to_string(),
            expires_at,
        })
    }

    pub async fn complete_authorization(
        &self,
        binding: McpOAuthBinding,
        flow_id: Uuid,
        returned_state: &str,
        authorization_code: &str,
        now: DateTime<Utc>,
    ) -> Result<ResolvedMcpOAuthCredential, McpOAuthError> {
        binding.validate(self.loopback_permitted)?;
        if flow_id.is_nil()
            || returned_state.is_empty()
            || returned_state.len() > MAX_FIELD_BYTES
            || authorization_code.is_empty()
            || authorization_code.len() > MAX_SECRET_BYTES
        {
            return Err(McpOAuthError::InvalidAuthorizationCallback);
        }
        let mut lease = self.acquire(binding).await?;
        let current = lease.load()?.ok_or(McpOAuthError::AuthorizationRequired)?;
        let StoredCredentialState::PendingAuthorization {
            flow_id: stored_flow_id,
            state,
            verifier,
            token_endpoint,
            client_id,
            redirect_uri,
            scopes,
            expires_at_ms,
            revocation_endpoint,
            ..
        } = &current.state
        else {
            return Err(McpOAuthError::InvalidAuthorizationCallback);
        };
        if *stored_flow_id != flow_id
            || !constant_time_eq(state.as_bytes(), returned_state.as_bytes())
        {
            return Err(McpOAuthError::InvalidAuthorizationCallback);
        }
        if now.timestamp_millis() > *expires_at_ms {
            let expired =
                authorization_required_record(&current, McpOAuthAuthorizationReason::FlowExpired);
            lease.persist(Some(current.revision), &expired)?;
            return Err(McpOAuthError::InvalidAuthorizationCallback);
        }
        let exchange = TokenExchange {
            token_endpoint: token_endpoint.clone(),
            client_id: client_id.clone(),
            redirect_uri: redirect_uri.clone(),
            verifier: Zeroizing::new(verifier.clone()),
            code: Zeroizing::new(authorization_code.to_owned()),
            scopes: scopes.clone(),
            revocation_endpoint: revocation_endpoint.clone(),
        };
        let intent = StoredCredentialRecord {
            schema_version: STORE_SCHEMA_VERSION,
            revision: current.revision + 1,
            binding: current.binding.clone(),
            state: StoredCredentialState::Exchanging {
                operation_id: Uuid::now_v7(),
                started_at_ms: now.timestamp_millis(),
            },
        };
        lease.persist(Some(current.revision), &intent)?;
        let coordinator = self.clone();
        let task = tokio::spawn(async move {
            coordinator
                .exchange_authorization(lease, intent, exchange, now)
                .await
        });
        task.await.map_err(|_| McpOAuthError::ProviderUnavailable)?
    }

    pub async fn resolve_access_token(
        &self,
        binding: McpOAuthBinding,
        now: DateTime<Utc>,
    ) -> Result<ResolvedMcpOAuthCredential, McpOAuthError> {
        binding.validate(self.loopback_permitted)?;
        let mut lease = self.acquire(binding).await?;
        let current = lease.load()?.ok_or(McpOAuthError::AuthorizationRequired)?;
        let StoredCredentialState::Active(active) = &current.state else {
            if matches!(
                current.state,
                StoredCredentialState::Exchanging { .. } | StoredCredentialState::Refreshing { .. }
            ) {
                recover_indeterminate(&mut lease, &current)?;
            }
            return Err(McpOAuthError::AuthorizationRequired);
        };
        if active
            .expires_at_ms
            .is_none_or(|expires_at| expires_at > now.timestamp_millis() + REFRESH_SKEW_MS)
        {
            return Ok(resolved(active, current.revision));
        }
        let Some(refresh_token) = active
            .refresh_token
            .as_ref()
            .filter(|token| !token.is_empty())
        else {
            let next = authorization_required_record(
                &current,
                McpOAuthAuthorizationReason::NoRefreshToken,
            );
            lease.persist(Some(current.revision), &next)?;
            return Err(McpOAuthError::AuthorizationRequired);
        };
        let refresh = TokenRefresh {
            token_endpoint: active.token_endpoint.clone(),
            client_id: active.client_id.clone(),
            refresh_token: Zeroizing::new(refresh_token.clone()),
            previous_scopes: active.scopes.clone(),
            revocation_endpoint: active.revocation_endpoint.clone(),
        };
        let intent = StoredCredentialRecord {
            schema_version: STORE_SCHEMA_VERSION,
            revision: current.revision + 1,
            binding: current.binding.clone(),
            state: StoredCredentialState::Refreshing {
                operation_id: Uuid::now_v7(),
                started_at_ms: now.timestamp_millis(),
            },
        };
        lease.persist(Some(current.revision), &intent)?;
        let coordinator = self.clone();
        let task =
            tokio::spawn(async move { coordinator.refresh(lease, intent, refresh, now).await });
        task.await.map_err(|_| McpOAuthError::ProviderUnavailable)?
    }

    pub async fn status(
        &self,
        binding: McpOAuthBinding,
    ) -> Result<McpOAuthCredentialStatus, McpOAuthError> {
        binding.validate(self.loopback_permitted)?;
        let mut lease = self.acquire(binding).await?;
        let Some(record) = lease.load()? else {
            return Ok(McpOAuthCredentialStatus::Absent);
        };
        if matches!(
            record.state,
            StoredCredentialState::Exchanging { .. } | StoredCredentialState::Refreshing { .. }
        ) {
            let reason = recover_indeterminate(&mut lease, &record)?;
            return Ok(McpOAuthCredentialStatus::AuthorizationRequired {
                reason,
                revision: record.revision + 1,
            });
        }
        if matches!(
            record.state,
            StoredCredentialState::PendingAuthorization { expires_at_ms, .. }
                if expires_at_ms < Utc::now().timestamp_millis()
        ) {
            let expired =
                authorization_required_record(&record, McpOAuthAuthorizationReason::FlowExpired);
            lease.persist(Some(record.revision), &expired)?;
            return Ok(McpOAuthCredentialStatus::AuthorizationRequired {
                reason: McpOAuthAuthorizationReason::FlowExpired,
                revision: expired.revision,
            });
        }
        Ok(status_from_record(&record))
    }

    pub async fn record_rejected_access_token(
        &self,
        binding: McpOAuthBinding,
        rejected_token_digest: &str,
    ) -> Result<bool, McpOAuthError> {
        binding.validate(self.loopback_permitted)?;
        if rejected_token_digest.len() != 64 {
            return Ok(false);
        }
        let mut lease = self.acquire(binding).await?;
        let Some(current) = lease.load()? else {
            return Ok(false);
        };
        let StoredCredentialState::Active(active) = &current.state else {
            return Ok(false);
        };
        if !constant_time_eq(
            token_digest(&active.access_token).as_bytes(),
            rejected_token_digest.as_bytes(),
        ) {
            return Ok(false);
        }
        let next = authorization_required_record(
            &current,
            McpOAuthAuthorizationReason::AccessTokenRejected,
        );
        lease.persist(Some(current.revision), &next)?;
        Ok(true)
    }

    /// Revokes locally, then tells the provider if it published an endpoint.
    ///
    /// The order is the whole point. Local `Revoked` commits first, so a
    /// provider that is unreachable, slow or hostile cannot leave the credential
    /// usable here. The remote call is best effort by contract and its failure
    /// is reported rather than retried into a state change.
    pub async fn revoke(
        &self,
        binding: McpOAuthBinding,
    ) -> Result<McpOAuthRevocation, McpOAuthError> {
        binding.validate(self.loopback_permitted)?;
        let mut lease = self.acquire(binding.clone()).await?;
        let current = lease.load()?;
        // Captured before the record is replaced; afterwards the token is gone.
        let notify = current.as_ref().and_then(|record| match &record.state {
            StoredCredentialState::Active(active) => {
                active
                    .revocation_endpoint
                    .as_ref()
                    .map(|endpoint| RemoteRevocation {
                        endpoint: endpoint.clone(),
                        client_id: active.client_id.clone(),
                        // RFC 7009 prefers the refresh token: revoking it invalidates
                        // the whole grant rather than one access token.
                        is_refresh: active.refresh_token.is_some(),
                        token: Zeroizing::new(
                            active
                                .refresh_token
                                .clone()
                                .unwrap_or_else(|| active.access_token.clone()),
                        ),
                    })
            }
            _ => None,
        });
        let record = StoredCredentialRecord {
            schema_version: STORE_SCHEMA_VERSION,
            revision: current.as_ref().map_or(1, |record| record.revision + 1),
            binding: StoredBinding::from(&binding),
            state: StoredCredentialState::Revoked,
        };
        lease.persist(current.as_ref().map(|record| record.revision), &record)?;
        // The lease is released before the network call: local state is already
        // committed, and holding a credential-scoped lock across an outbound
        // request would let a slow provider block every other operation on it.
        drop(lease);

        let remote_confirmed = match notify {
            Some(notify) => self.revoke_remote(&notify).await,
            None => false,
        };
        Ok(McpOAuthRevocation {
            revision: record.revision,
            remote_confirmed,
        })
    }

    /// RFC 7009 token revocation. Returns whether the provider confirmed.
    async fn revoke_remote(&self, notify: &RemoteRevocation) -> bool {
        let Ok(http) = crate::mcp::build_pinned_http_client_for_endpoint(
            &notify.endpoint,
            self.request_timeout,
            self.loopback_permitted,
        ) else {
            return false;
        };
        let form = [
            ("token", notify.token.as_str()),
            (
                "token_type_hint",
                if notify.is_refresh {
                    "refresh_token"
                } else {
                    "access_token"
                },
            ),
            ("client_id", notify.client_id.as_str()),
        ];
        let Ok(Ok(response)) = tokio::time::timeout(
            self.request_timeout,
            http.post(&notify.endpoint)
                .header(reqwest::header::ACCEPT, "application/json")
                .form(&form)
                .send(),
        )
        .await
        else {
            return false;
        };
        response.status().is_success()
    }

    async fn acquire(&self, binding: McpOAuthBinding) -> Result<CredentialLease, McpOAuthError> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.acquire(binding))
            .await
            .map_err(|_| McpOAuthError::StoreUnavailable)?
    }

    async fn exchange_authorization(
        &self,
        mut lease: CredentialLease,
        intent: StoredCredentialRecord,
        exchange: TokenExchange,
        now: DateTime<Utc>,
    ) -> Result<ResolvedMcpOAuthCredential, McpOAuthError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", exchange.code.as_str()),
            ("client_id", exchange.client_id.as_str()),
            ("redirect_uri", exchange.redirect_uri.as_str()),
            ("code_verifier", exchange.verifier.as_str()),
        ];
        let response = self.request_token(&exchange.token_endpoint, &form).await;
        self.finish_token_request(
            &mut lease,
            intent,
            response,
            exchange.token_endpoint,
            exchange.client_id,
            exchange.scopes,
            None,
            exchange.revocation_endpoint,
            now,
            McpOAuthAuthorizationReason::ExchangeIndeterminate,
        )
    }

    async fn refresh(
        &self,
        mut lease: CredentialLease,
        intent: StoredCredentialRecord,
        refresh: TokenRefresh,
        now: DateTime<Utc>,
    ) -> Result<ResolvedMcpOAuthCredential, McpOAuthError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.refresh_token.as_str()),
            ("client_id", refresh.client_id.as_str()),
        ];
        let response = self.request_token(&refresh.token_endpoint, &form).await;
        self.finish_token_request(
            &mut lease,
            intent,
            response,
            refresh.token_endpoint,
            refresh.client_id,
            refresh.previous_scopes,
            Some(refresh.refresh_token.as_str()),
            refresh.revocation_endpoint.clone(),
            now,
            McpOAuthAuthorizationReason::RefreshIndeterminate,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_token_request(
        &self,
        lease: &mut CredentialLease,
        intent: StoredCredentialRecord,
        response: Result<TokenResponse, McpOAuthError>,
        token_endpoint: String,
        client_id: String,
        previous_scopes: Vec<String>,
        previous_refresh_token: Option<&str>,
        revocation_endpoint: Option<String>,
        now: DateTime<Utc>,
        indeterminate_reason: McpOAuthAuthorizationReason,
    ) -> Result<ResolvedMcpOAuthCredential, McpOAuthError> {
        match response {
            Ok(tokens) => {
                let active = match active_from_response(
                    tokens,
                    token_endpoint,
                    client_id,
                    previous_scopes,
                    previous_refresh_token,
                    revocation_endpoint,
                    now,
                ) {
                    Ok(active) => active,
                    Err(error) => {
                        let failed = authorization_required_record(&intent, indeterminate_reason);
                        lease.persist(Some(intent.revision), &failed)?;
                        return Err(error);
                    }
                };
                let committed = StoredCredentialRecord {
                    schema_version: STORE_SCHEMA_VERSION,
                    revision: intent.revision + 1,
                    binding: intent.binding.clone(),
                    state: StoredCredentialState::Active(active),
                };
                lease.persist(Some(intent.revision), &committed)?;
                let StoredCredentialState::Active(active) = &committed.state else {
                    unreachable!()
                };
                Ok(resolved(active, committed.revision))
            }
            Err(error) => {
                let reason = if matches!(error, McpOAuthError::ProviderRejected) {
                    McpOAuthAuthorizationReason::ProviderRejected
                } else {
                    indeterminate_reason
                };
                let failed = authorization_required_record(&intent, reason);
                lease.persist(Some(intent.revision), &failed)?;
                Err(error)
            }
        }
    }

    async fn request_token(
        &self,
        token_endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, McpOAuthError> {
        let http = crate::mcp::build_pinned_http_client_for_endpoint(
            token_endpoint,
            self.request_timeout,
            self.loopback_permitted,
        )
        .map_err(|_| McpOAuthError::ProviderUnavailable)?;
        let response = tokio::time::timeout(
            self.request_timeout,
            http.post(token_endpoint)
                .header(reqwest::header::ACCEPT, "application/json")
                .form(form)
                .send(),
        )
        .await
        .map_err(|_| McpOAuthError::ProviderUnavailable)?
        .map_err(|_| McpOAuthError::ProviderUnavailable)?;
        if response
            .content_length()
            .is_some_and(|length| length as usize > MAX_TOKEN_RESPONSE_BYTES)
        {
            return Err(McpOAuthError::ProviderUnavailable);
        }
        let status = response.status();
        let body = bounded_body(response, MAX_TOKEN_RESPONSE_BYTES).await?;
        if !status.is_success() {
            return Err(if status.is_client_error() {
                McpOAuthError::ProviderRejected
            } else {
                McpOAuthError::ProviderUnavailable
            });
        }
        serde_json::from_slice(&body).map_err(|_| McpOAuthError::ProviderUnavailable)
    }
}

struct TokenExchange {
    token_endpoint: String,
    client_id: String,
    redirect_uri: String,
    verifier: Zeroizing<String>,
    code: Zeroizing<String>,
    scopes: Vec<String>,
    revocation_endpoint: Option<String>,
}

struct TokenRefresh {
    token_endpoint: String,
    client_id: String,
    refresh_token: Zeroizing<String>,
    previous_scopes: Vec<String>,
    revocation_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(refresh_token) = &mut self.refresh_token {
            refresh_token.zeroize();
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct StoredBinding {
    tenant_id: Uuid,
    server_id: Uuid,
    credential_id: Uuid,
    endpoint: String,
}

impl From<&McpOAuthBinding> for StoredBinding {
    fn from(binding: &McpOAuthBinding) -> Self {
        Self {
            tenant_id: binding.tenant_id,
            server_id: binding.server_id,
            credential_id: binding.credential_id,
            endpoint: binding.endpoint.clone(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoredCredentialRecord {
    schema_version: u32,
    revision: u64,
    binding: StoredBinding,
    state: StoredCredentialState,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum StoredCredentialState {
    PendingAuthorization {
        flow_id: Uuid,
        state: String,
        verifier: String,
        authorization_endpoint: String,
        token_endpoint: String,
        client_id: String,
        redirect_uri: String,
        scopes: Vec<String>,
        expires_at_ms: i64,
        /// Defaulted so records written before remote revocation existed still
        /// decode; they simply have no endpoint to notify.
        #[serde(default)]
        revocation_endpoint: Option<String>,
    },
    Exchanging {
        operation_id: Uuid,
        started_at_ms: i64,
    },
    Active(ActiveCredential),
    Refreshing {
        operation_id: Uuid,
        started_at_ms: i64,
    },
    AuthorizationRequired {
        reason: McpOAuthAuthorizationReason,
    },
    Revoked,
}

impl Drop for StoredCredentialState {
    fn drop(&mut self) {
        if let Self::PendingAuthorization {
            state, verifier, ..
        } = self
        {
            state.zeroize();
            verifier.zeroize();
        }
    }
}

#[derive(Deserialize, Serialize)]
struct ActiveCredential {
    token_endpoint: String,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: Option<i64>,
    scopes: Vec<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

impl Drop for ActiveCredential {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(refresh_token) = &mut self.refresh_token {
            refresh_token.zeroize();
        }
    }
}

#[derive(Deserialize, Serialize)]
struct SealedCredentialRecord {
    schema_version: u32,
    nonce_base64: String,
    ciphertext_base64: String,
}

struct EncryptedFileCredentialStore {
    root: PathBuf,
    master_key: Zeroizing<[u8; 32]>,
}

impl EncryptedFileCredentialStore {
    fn new(root: &Path, master_key: [u8; 32]) -> Result<Self, McpOAuthError> {
        if !root.is_absolute() {
            return Err(McpOAuthError::StoreUnavailable);
        }
        if let Ok(metadata) = std::fs::symlink_metadata(root)
            && metadata.file_type().is_symlink()
        {
            return Err(McpOAuthError::StoreUnavailable);
        }
        std::fs::create_dir_all(root).map_err(|_| McpOAuthError::StoreUnavailable)?;
        set_owner_only_directory(root)?;
        let root = std::fs::canonicalize(root).map_err(|_| McpOAuthError::StoreUnavailable)?;
        Ok(Self {
            root,
            master_key: Zeroizing::new(master_key),
        })
    }

    fn acquire(
        self: Arc<Self>,
        binding: McpOAuthBinding,
    ) -> Result<CredentialLease, McpOAuthError> {
        let directory = self.binding_directory(&binding)?;
        let lock_path = directory.join(format!("{}.lock", binding.credential_id));
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        set_owner_only_file(&lock_file)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: the descriptor remains owned by the returned lease. The
            // OS releases the lock after normal Drop or process death.
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(McpOAuthError::StoreUnavailable);
            }
        }
        #[cfg(not(unix))]
        {
            return Err(McpOAuthError::StoreUnavailable);
        }
        Ok(CredentialLease {
            store: self,
            binding,
            directory,
            lock_file,
        })
    }

    fn binding_directory(&self, binding: &McpOAuthBinding) -> Result<PathBuf, McpOAuthError> {
        let tenant = self.root.join(binding.tenant_id.to_string());
        let server = tenant.join(binding.server_id.to_string());
        for directory in [&tenant, &server] {
            if let Ok(metadata) = std::fs::symlink_metadata(directory)
                && metadata.file_type().is_symlink()
            {
                return Err(McpOAuthError::StoreUnavailable);
            }
            std::fs::create_dir_all(directory).map_err(|_| McpOAuthError::StoreUnavailable)?;
            set_owner_only_directory(directory)?;
        }
        Ok(server)
    }
}

struct CredentialLease {
    store: Arc<EncryptedFileCredentialStore>,
    binding: McpOAuthBinding,
    directory: PathBuf,
    lock_file: File,
}

impl CredentialLease {
    fn record_path(&self) -> PathBuf {
        self.directory
            .join(format!("{}.json", self.binding.credential_id))
    }

    fn load(&self) -> Result<Option<StoredCredentialRecord>, McpOAuthError> {
        let path = self.record_path();
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(McpOAuthError::StoreUnavailable),
        };
        let length = file
            .metadata()
            .map_err(|_| McpOAuthError::StoreUnavailable)?
            .len() as usize;
        if length == 0 || length > MAX_RECORD_BYTES {
            return Err(McpOAuthError::StoreUnavailable);
        }
        let mut encoded = Vec::with_capacity(length);
        file.read_to_end(&mut encoded)
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        let sealed: SealedCredentialRecord =
            serde_json::from_slice(&encoded).map_err(|_| McpOAuthError::StoreUnavailable)?;
        if sealed.schema_version != SEALED_SCHEMA_VERSION {
            return Err(McpOAuthError::StoreUnavailable);
        }
        let nonce = base64::engine::general_purpose::STANDARD
            .decode(sealed.nonce_base64)
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(sealed.ciphertext_base64)
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        if nonce.len() != 12 || ciphertext.is_empty() {
            return Err(McpOAuthError::StoreUnavailable);
        }
        let cipher = Aes256Gcm::new_from_slice(self.store.master_key.as_slice())
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        let aad = self.binding.aad();
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| McpOAuthError::StoreUnavailable)?,
        );
        let record: StoredCredentialRecord = serde_json::from_slice(plaintext.as_slice())
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        if record.schema_version != STORE_SCHEMA_VERSION
            || record.revision == 0
            || record.binding != StoredBinding::from(&self.binding)
        {
            return Err(McpOAuthError::StoreUnavailable);
        }
        Ok(Some(record))
    }

    fn persist(
        &mut self,
        expected_revision: Option<u64>,
        record: &StoredCredentialRecord,
    ) -> Result<(), McpOAuthError> {
        let observed = self.load()?.map(|record| record.revision);
        if observed != expected_revision
            || record.revision != expected_revision.map_or(1, |revision| revision + 1)
            || record.binding != StoredBinding::from(&self.binding)
        {
            return Err(McpOAuthError::StoreUnavailable);
        }
        let plaintext = Zeroizing::new(
            serde_json::to_vec(record).map_err(|_| McpOAuthError::StoreUnavailable)?,
        );
        let mut nonce = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(self.store.master_key.as_slice())
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        let aad = self.binding.aad();
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_slice(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        let sealed = SealedCredentialRecord {
            schema_version: SEALED_SCHEMA_VERSION,
            nonce_base64: base64::engine::general_purpose::STANDARD.encode(nonce),
            ciphertext_base64: base64::engine::general_purpose::STANDARD.encode(ciphertext),
        };
        let encoded = serde_json::to_vec(&sealed).map_err(|_| McpOAuthError::StoreUnavailable)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(McpOAuthError::StoreUnavailable);
        }
        let path = self.record_path();
        let staging = path.with_extension("json.partial");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&staging)
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        set_owner_only_file(&file)?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
        std::fs::rename(&staging, &path).map_err(|_| McpOAuthError::StoreUnavailable)?;
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| McpOAuthError::StoreUnavailable)
    }
}

#[cfg(unix)]
impl Drop for CredentialLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is valid for this guard's lifetime.
        let _ = unsafe { libc::flock(self.lock_file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn validate_authorization_request(
    request: &McpOAuthAuthorizationRequest,
    loopback_permitted: bool,
) -> Result<Url, McpOAuthError> {
    if request.client_id.is_empty()
        || request.client_id.len() > MAX_FIELD_BYTES
        || request.redirect_uri.is_empty()
        || request.redirect_uri.len() > MAX_FIELD_BYTES
        || request.scopes.is_empty()
        || request.scopes.len() > MAX_SCOPES
    {
        return Err(McpOAuthError::InvalidAuthorizationRequest);
    }
    let scopes = request.scopes.iter().collect::<BTreeSet<_>>();
    if scopes.len() != request.scopes.len()
        || request.scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 256
                || scope.chars().any(char::is_whitespace)
                || scope.chars().any(char::is_control)
        })
    {
        return Err(McpOAuthError::InvalidAuthorizationRequest);
    }
    crate::mcp::require_permitted_endpoint(&request.authorization_endpoint, loopback_permitted)
        .map_err(|_| McpOAuthError::InvalidAuthorizationRequest)?;
    crate::mcp::require_permitted_endpoint(&request.token_endpoint, loopback_permitted)
        .map_err(|_| McpOAuthError::InvalidAuthorizationRequest)?;
    if let Some(revocation_endpoint) = &request.revocation_endpoint {
        if revocation_endpoint.is_empty() || revocation_endpoint.len() > MAX_FIELD_BYTES {
            return Err(McpOAuthError::InvalidAuthorizationRequest);
        }
        crate::mcp::require_permitted_endpoint(revocation_endpoint, loopback_permitted)
            .map_err(|_| McpOAuthError::InvalidAuthorizationRequest)?;
    }
    let authorization_url = Url::parse(&request.authorization_endpoint)
        .map_err(|_| McpOAuthError::InvalidAuthorizationRequest)?;
    let redirect = Url::parse(&request.redirect_uri)
        .map_err(|_| McpOAuthError::InvalidAuthorizationRequest)?;
    if authorization_url.fragment().is_some()
        || !authorization_url.username().is_empty()
        || authorization_url.password().is_some()
        || !valid_public_client_redirect(&redirect)
    {
        return Err(McpOAuthError::InvalidAuthorizationRequest);
    }
    Ok(authorization_url)
}

fn valid_public_client_redirect(redirect: &Url) -> bool {
    if redirect.fragment().is_some()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
    {
        return false;
    }
    match redirect.scheme() {
        "https" => redirect.host_str().is_some(),
        "http" => redirect.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        }),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn active_from_response(
    mut response: TokenResponse,
    token_endpoint: String,
    client_id: String,
    previous_scopes: Vec<String>,
    previous_refresh_token: Option<&str>,
    revocation_endpoint: Option<String>,
    now: DateTime<Utc>,
) -> Result<ActiveCredential, McpOAuthError> {
    if response.access_token.is_empty()
        || response.access_token.len() > MAX_SECRET_BYTES
        || response
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        || response
            .refresh_token
            .as_ref()
            .is_some_and(|token| token.is_empty() || token.len() > MAX_SECRET_BYTES)
        || response
            .expires_in
            .is_some_and(|seconds| seconds > 366 * 24 * 60 * 60)
    {
        return Err(McpOAuthError::ProviderUnavailable);
    }
    let scopes: Vec<String> = match response.scope.take() {
        Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
        None => previous_scopes,
    };
    if scopes.len() > MAX_SCOPES
        || scopes.iter().any(|scope| {
            scope.is_empty() || scope.len() > 256 || scope.chars().any(char::is_control)
        })
    {
        return Err(McpOAuthError::ProviderUnavailable);
    }
    let refresh_token = response
        .refresh_token
        .take()
        .or_else(|| previous_refresh_token.map(str::to_owned));
    Ok(ActiveCredential {
        token_endpoint,
        client_id,
        access_token: std::mem::take(&mut response.access_token),
        refresh_token,
        expires_at_ms: response
            .expires_in
            .and_then(|seconds| i64::try_from(seconds).ok())
            .and_then(|seconds| now.timestamp_millis().checked_add(seconds * 1_000)),
        scopes,
        revocation_endpoint,
    })
}

fn resolved(active: &ActiveCredential, revision: u64) -> ResolvedMcpOAuthCredential {
    ResolvedMcpOAuthCredential {
        access_token: Zeroizing::new(active.access_token.clone()),
        token_digest: token_digest(&active.access_token),
        revision,
    }
}

fn token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn authorization_required_record(
    current: &StoredCredentialRecord,
    reason: McpOAuthAuthorizationReason,
) -> StoredCredentialRecord {
    StoredCredentialRecord {
        schema_version: STORE_SCHEMA_VERSION,
        revision: current.revision + 1,
        binding: current.binding.clone(),
        state: StoredCredentialState::AuthorizationRequired { reason },
    }
}

fn recover_indeterminate(
    lease: &mut CredentialLease,
    current: &StoredCredentialRecord,
) -> Result<McpOAuthAuthorizationReason, McpOAuthError> {
    let reason = match current.state {
        StoredCredentialState::Exchanging { .. } => {
            McpOAuthAuthorizationReason::ExchangeIndeterminate
        }
        StoredCredentialState::Refreshing { .. } => {
            McpOAuthAuthorizationReason::RefreshIndeterminate
        }
        _ => return Err(McpOAuthError::StoreUnavailable),
    };
    let recovered = authorization_required_record(current, reason);
    lease.persist(Some(current.revision), &recovered)?;
    Ok(reason)
}

fn status_from_record(record: &StoredCredentialRecord) -> McpOAuthCredentialStatus {
    match &record.state {
        StoredCredentialState::PendingAuthorization { expires_at_ms, .. } => {
            McpOAuthCredentialStatus::PendingAuthorization {
                expires_at: DateTime::from_timestamp_millis(*expires_at_ms)
                    .unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
                revision: record.revision,
            }
        }
        StoredCredentialState::Active(active) => McpOAuthCredentialStatus::Active {
            expires_at: active
                .expires_at_ms
                .and_then(DateTime::from_timestamp_millis),
            revision: record.revision,
        },
        StoredCredentialState::AuthorizationRequired { reason } => {
            McpOAuthCredentialStatus::AuthorizationRequired {
                reason: *reason,
                revision: record.revision,
            }
        }
        StoredCredentialState::Revoked => McpOAuthCredentialStatus::Revoked {
            revision: record.revision,
        },
        StoredCredentialState::Exchanging { .. } | StoredCredentialState::Refreshing { .. } => {
            unreachable!("indeterminate state is recovered before status conversion")
        }
    }
}

/// RFC 9728 Protected Resource Metadata, reduced to the fields this stage acts
/// on. Unknown fields are ignored rather than rejected so a richer document from
/// a conformant server still works.
#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

impl ProtectedResourceMetadata {
    fn validate_capacity(&self) -> Result<(), McpOAuthError> {
        bounded_field(&self.resource)?;
        if self.authorization_servers.len() > MAX_SCOPES {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        self.authorization_servers
            .iter()
            .try_for_each(|issuer| bounded_field(issuer))?;
        bounded_scopes(&self.scopes_supported)
    }
}

/// RFC 8414 Authorization Server Metadata.
#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    response_types_supported: Vec<String>,
    /// RFC 7009. Optional: a provider that does not publish one simply cannot be
    /// told remotely, which never blocks local revocation.
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

impl AuthorizationServerMetadata {
    fn validate(&self, issuer: &Url, loopback_permitted: bool) -> Result<(), McpOAuthError> {
        bounded_field(&self.issuer)?;
        bounded_field(&self.authorization_endpoint)?;
        bounded_field(&self.token_endpoint)?;
        bounded_scopes(&self.scopes_supported)?;
        if !urls_equal(&self.issuer, issuer.as_str()) {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        // S256 only. An absent list is not permission to fall back to `plain`.
        if !self
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        // An implicit-only server cannot serve an authorization code flow.
        if !self.response_types_supported.is_empty()
            && !self
                .response_types_supported
                .iter()
                .any(|response| response == "code")
        {
            return Err(McpOAuthError::DiscoveryRejected);
        }
        // The revocation endpoint is held to the same rules as the others: a
        // provider may omit it, but if it names one, that URL receives a refresh
        // token and must be one the issuer actually owns.
        let mut endpoints = vec![&self.authorization_endpoint, &self.token_endpoint];
        if let Some(revocation) = &self.revocation_endpoint {
            bounded_field(revocation)?;
            endpoints.push(revocation);
        }
        for endpoint in endpoints {
            crate::mcp::require_permitted_endpoint(endpoint, loopback_permitted)
                .map_err(|_| McpOAuthError::DiscoveryRejected)?;
            let parsed = Url::parse(endpoint).map_err(|_| McpOAuthError::DiscoveryRejected)?;
            // The issuer must own its own endpoints. Without this a resource
            // that names a legitimate issuer could still redirect the code
            // exchange to an origin the issuer does not control.
            if !same_origin(issuer, &parsed)
                || parsed.fragment().is_some()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
            {
                return Err(McpOAuthError::DiscoveryRejected);
            }
        }
        Ok(())
    }
}

fn bounded_field(value: &str) -> Result<(), McpOAuthError> {
    if value.is_empty() || value.len() > MAX_FIELD_BYTES {
        return Err(McpOAuthError::DiscoveryRejected);
    }
    Ok(())
}

fn bounded_scopes(scopes: &[String]) -> Result<(), McpOAuthError> {
    if scopes.len() > MAX_SCOPES {
        return Err(McpOAuthError::DiscoveryRejected);
    }
    scopes.iter().try_for_each(|scope| bounded_field(scope))
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn urls_equal(left: &str, right: &str) -> bool {
    matches!((Url::parse(left), Url::parse(right)), (Ok(left), Ok(right)) if left == right)
}

/// RFC 8414 / RFC 9728 place the well-known segment between the authority and
/// the resource path, so a resource at `/mcp` is described at
/// `/.well-known/oauth-protected-resource/mcp`.
fn well_known_url(base: &Url, suffix: &str) -> Url {
    let path = base.path().trim_end_matches('/').to_owned();
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&format!("/.well-known/{suffix}{path}"));
    url
}

/// Extracts `resource_metadata` from a `WWW-Authenticate` challenge.
///
/// `Ok(None)` means the header carried no such parameter, which is an ordinary
/// Bearer challenge. Anything oversized, control-bearing or malformed is
/// refused outright rather than ignored, so a hostile header fails closed
/// instead of silently falling back to the default well-known path.
fn challenge_resource_metadata(raw: &str) -> Result<Option<String>, McpOAuthError> {
    if raw.len() > MAX_FIELD_BYTES || raw.chars().any(char::is_control) {
        return Err(McpOAuthError::DiscoveryRejected);
    }
    let Some(rest) = raw.split("resource_metadata=").nth(1) else {
        return Ok(None);
    };
    let value = rest
        .strip_prefix('"')
        .and_then(|rest| rest.split('"').next())
        .ok_or(McpOAuthError::DiscoveryRejected)?;
    bounded_field(value)?;
    Ok(Some(value.to_owned()))
}

fn negotiate_scopes(
    requested: &[String],
    supported: &[String],
) -> Result<Vec<String>, McpOAuthError> {
    if requested.is_empty() || requested.len() > MAX_SCOPES {
        return Err(McpOAuthError::InvalidAuthorizationRequest);
    }
    if supported.is_empty() {
        return Ok(requested.to_vec());
    }
    let available = supported.iter().collect::<BTreeSet<_>>();
    let granted = requested
        .iter()
        .filter(|scope| available.contains(scope))
        .cloned()
        .collect::<Vec<_>>();
    if granted.is_empty() {
        return Err(McpOAuthError::DiscoveryRejected);
    }
    Ok(granted)
}

async fn bounded_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, McpOAuthError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| McpOAuthError::ProviderUnavailable)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(McpOAuthError::ProviderUnavailable);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn set_owner_only_directory(path: &Path) -> Result<(), McpOAuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
    }
    Ok(())
}

fn set_owner_only_file(file: &File) -> Result<(), McpOAuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| McpOAuthError::StoreUnavailable)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);

    impl Drop for TestRoot {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[tokio::test]
    async fn a_crashed_refresh_intent_requires_reauthorization_and_is_never_replayed() {
        let root = TestRoot(
            std::env::temp_dir().join(format!("agent-mcp-oauth-crash-{}", Uuid::now_v7())),
        );
        let key = [19_u8; 32];
        let coordinator =
            McpOAuthCoordinator::new(&root.0, key, Duration::from_secs(1), true).unwrap();
        let binding = McpOAuthBinding {
            tenant_id: Uuid::now_v7(),
            server_id: Uuid::now_v7(),
            credential_id: Uuid::now_v7(),
            endpoint: "http://127.0.0.1:1/mcp".into(),
        };
        let mut lease = coordinator.acquire(binding.clone()).await.unwrap();
        lease
            .persist(
                None,
                &StoredCredentialRecord {
                    schema_version: STORE_SCHEMA_VERSION,
                    revision: 1,
                    binding: StoredBinding::from(&binding),
                    state: StoredCredentialState::Refreshing {
                        operation_id: Uuid::now_v7(),
                        started_at_ms: Utc::now().timestamp_millis(),
                    },
                },
            )
            .unwrap();
        drop(lease);
        drop(coordinator);

        let restarted =
            McpOAuthCoordinator::new(&root.0, key, Duration::from_secs(1), true).unwrap();
        let expected = McpOAuthCredentialStatus::AuthorizationRequired {
            reason: McpOAuthAuthorizationReason::RefreshIndeterminate,
            revision: 2,
        };
        assert_eq!(restarted.status(binding.clone()).await.unwrap(), expected);
        assert_eq!(
            restarted.status(binding).await.unwrap(),
            expected,
            "recovery must converge once rather than replay or keep advancing"
        );
    }
}
