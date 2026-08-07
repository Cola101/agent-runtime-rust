//! Federated MCP tool calls (ADR-0040).
//!
//! This lives in the gateway rather than the Worker for one reason: the gateway
//! is where sealed credentials are opened. Putting it in the Worker would hand a
//! tenant's MCP credential to the process that is executing a model's
//! suggestions, which is the one place it must never be.
//!
//! v1 is HTTP only. No local process is spawned, so third-party code runs on the
//! third party's machine and what crosses this boundary is a request and a
//! response.

use crate::ProviderExecutionError;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use rsa::pkcs8::DecodePrivateKey;
use rsa::{Oaep, RsaPrivateKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;
use zeroize::Zeroizing;

const ENVELOPE_ALGORITHM: &str = "RSA-OAEP-256+A256GCM";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// A federated result is untrusted third-party content headed for the model's
/// context. Unbounded, one server could exhaust the Run's context or the
/// gateway's memory by answering a one-word question with a gigabyte.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// A server advertising thousands of tools would push everything else out of
/// the prompt. Sixty-four is already more than a Skill can plausibly declare and
/// reason about, so this bounds damage rather than expressing a target.
const MAX_TOOLS: usize = 64;

#[derive(Clone, Debug)]
pub struct McpServerRef {
    pub server_id: Uuid,
    /// Namespace in qualified tool names: `mcp:<name>/<tool>`.
    pub name: String,
    pub endpoint: String,
    /// Sealed. JSON of the envelope, opened here and nowhere else.
    pub credential_envelope_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpTool {
    /// Always `mcp:<server>/<tool>`, so a federated tool can never be named the
    /// same as a native one whose safety this platform vouches for.
    pub qualified_name: String,
    pub description: String,
    pub input_schema_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpCatalog {
    pub tools: Vec<McpTool>,
    /// Digest of the catalog as discovered. A Run freezes this at start and
    /// presents it on every call; a server whose catalog changed underneath does
    /// not get to change what the Run may do.
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpToolResult {
    pub content_json: String,
    /// The server reported the call failed. Distinct from a transport failure:
    /// the model should see this, and nothing should be retried.
    pub is_error: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum McpFederationError {
    #[error("mcp server credential envelope could not be opened")]
    CredentialUnopenable,
    #[error("mcp server endpoint is not permitted: {0}")]
    EndpointNotPermitted(String),
    #[error("mcp server was unreachable: {0}")]
    Unreachable(String),
    #[error("mcp server returned a malformed response: {0}")]
    Protocol(String),
    #[error("mcp server response exceeded {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("mcp tool {0} is not in the catalog this Run froze")]
    ToolNotInFrozenCatalog(String),
    #[error("mcp server catalog changed after this Run froze it")]
    CatalogChanged,
}

impl From<McpFederationError> for ProviderExecutionError {
    fn from(error: McpFederationError) -> Self {
        ProviderExecutionError::InvalidConfiguration(error.to_string())
    }
}

#[derive(Deserialize)]
struct CredentialEnvelope {
    schema_version: u32,
    key_id: String,
    algorithm: String,
    encrypted_key: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    #[serde(default)]
    message: String,
}

#[derive(Clone)]
pub struct McpFederationClient {
    private_key: RsaPrivateKey,
    key_id: String,
    http: reqwest::Client,
}

impl McpFederationClient {
    pub fn from_pkcs8_pem(
        pem: &str,
        key_id: impl Into<String>,
        request_timeout: Duration,
    ) -> Result<Self, McpFederationError> {
        let private_key = RsaPrivateKey::from_pkcs8_pem(pem)
            .map_err(|_| McpFederationError::CredentialUnopenable)?;
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            // A federated server is the only host reachable for its own calls,
            // so a redirect to somewhere else is the exact thing the endpoint
            // field exists to prevent.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        Ok(Self {
            private_key,
            key_id: key_id.into(),
            http,
        })
    }

    /// Discovers what the server offers, qualified and digested.
    pub async fn list_tools(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
    ) -> Result<McpCatalog, McpFederationError> {
        let credential = self.open_credential(tenant_id, server)?;
        self.initialize(server, credential.as_deref().map(String::as_str)).await?;
        let result = self
            .call_json_rpc(server, credential.as_deref().map(String::as_str), "tools/list", serde_json::json!({}))
            .await?;
        let listed = result
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| McpFederationError::Protocol("tools/list returned no tools".into()))?;
        if listed.len() > MAX_TOOLS {
            return Err(McpFederationError::Protocol(format!(
                "server advertised {} tools, more than the {MAX_TOOLS} a catalog may hold",
                listed.len()
            )));
        }
        let mut tools = Vec::with_capacity(listed.len());
        for entry in listed {
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| McpFederationError::Protocol("a tool has no name".into()))?;
            // A tool name carrying the separator could make a call to one tool
            // resolve as another once qualified. The server chose this string,
            // so it is checked here rather than assumed.
            if name.is_empty() || name.contains('/') || name.contains(':') || name.len() > 128 {
                return Err(McpFederationError::Protocol(format!(
                    "tool name {name:?} cannot be qualified unambiguously"
                )));
            }
            tools.push(McpTool {
                qualified_name: format!("mcp:{}/{}", server.name, name),
                description: entry
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_schema_json: entry
                    .get("inputSchema")
                    .map(serde_json::Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned()),
            });
        }
        tools.sort_by(|left, right| left.qualified_name.cmp(&right.qualified_name));
        let digest = catalog_digest(&tools);
        Ok(McpCatalog { tools, digest })
    }

    /// Calls a tool, refusing if the server's catalog no longer matches what the
    /// Run froze.
    ///
    /// The re-discovery is the point: a server that adds or changes a tool
    /// mid-Run must not have that change take effect inside a Run that was
    /// approved against the old catalog. Codex enforces the same rule, and
    /// Checkpoint binding here already does for native Tools.
    pub async fn call_tool(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        qualified_name: &str,
        arguments_json: &str,
        frozen_catalog_digest: &str,
    ) -> Result<McpToolResult, McpFederationError> {
        let catalog = self.list_tools(tenant_id, server).await?;
        if catalog.digest != frozen_catalog_digest {
            return Err(McpFederationError::CatalogChanged);
        }
        if !catalog
            .tools
            .iter()
            .any(|tool| tool.qualified_name == qualified_name)
        {
            return Err(McpFederationError::ToolNotInFrozenCatalog(
                qualified_name.to_owned(),
            ));
        }
        let bare = qualified_name
            .rsplit_once('/')
            .map(|(_, tool)| tool)
            .ok_or_else(|| {
                McpFederationError::ToolNotInFrozenCatalog(qualified_name.to_owned())
            })?;
        let arguments: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
        let credential = self.open_credential(tenant_id, server)?;
        let result = self
            .call_json_rpc(
                server,
                credential.as_deref().map(String::as_str),
                "tools/call",
                serde_json::json!({ "name": bare, "arguments": arguments }),
            )
            .await?;
        Ok(McpToolResult {
            is_error: result
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            content_json: result
                .get("content")
                .map(serde_json::Value::to_string)
                .unwrap_or_else(|| "[]".to_owned()),
        })
    }

    async fn initialize(
        &self,
        server: &McpServerRef,
        credential: Option<&str>,
    ) -> Result<(), McpFederationError> {
        self.call_json_rpc(
            server,
            credential,
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "agent-runtime-platform", "version": "1" }
            }),
        )
        .await
        .map(|_| ())
    }

    async fn call_json_rpc(
        &self,
        server: &McpServerRef,
        credential: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpFederationError> {
        require_permitted_endpoint(&server.endpoint)?;
        let mut request = self
            .http
            .post(&server.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }));
        if let Some(secret) = credential {
            request = request.bearer_auth(secret);
        }
        let response = request
            .send()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        let status = response.status();
        // Read the length hint first so an oversized body is refused before it
        // is pulled into memory, not after.
        if response
            .content_length()
            .is_some_and(|length| length as usize > MAX_RESPONSE_BYTES)
        {
            return Err(McpFederationError::ResponseTooLarge);
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        // And again on what actually arrived: a server can omit or lie about
        // Content-Length, and chunked responses have none at all.
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(McpFederationError::ResponseTooLarge);
        }
        if !status.is_success() {
            return Err(McpFederationError::Unreachable(format!(
                "server answered HTTP {}",
                status.as_u16()
            )));
        }
        let decoded: JsonRpcResponse = serde_json::from_slice(&body)
            .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
        if let Some(error) = decoded.error {
            return Err(McpFederationError::Protocol(error.message));
        }
        decoded
            .result
            .ok_or_else(|| McpFederationError::Protocol("response carried neither result nor error".into()))
    }

    fn open_credential(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
    ) -> Result<Option<Zeroizing<String>>, McpFederationError> {
        // An open server is registered without a credential. That is a real
        // configuration, not a missing one, so it is not an error.
        if server.credential_envelope_json.trim().is_empty() {
            return Ok(None);
        }
        let envelope: CredentialEnvelope =
            serde_json::from_str(&server.credential_envelope_json)
                .map_err(|_| McpFederationError::CredentialUnopenable)?;
        if envelope.schema_version != 1
            || envelope.algorithm != ENVELOPE_ALGORITHM
            || envelope.key_id != self.key_id
        {
            return Err(McpFederationError::CredentialUnopenable);
        }
        let decode = |value: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(value)
                .map_err(|_| McpFederationError::CredentialUnopenable)
        };
        let encrypted_key = decode(&envelope.encrypted_key)?;
        let nonce = decode(&envelope.nonce)?;
        let ciphertext = decode(&envelope.ciphertext)?;
        if nonce.len() != 12 || ciphertext.is_empty() {
            return Err(McpFederationError::CredentialUnopenable);
        }
        let data_key = Zeroizing::new(
            self.private_key
                .decrypt(Oaep::new::<Sha256>(), &encrypted_key)
                .map_err(|_| McpFederationError::CredentialUnopenable)?,
        );
        if data_key.len() != 32 {
            return Err(McpFederationError::CredentialUnopenable);
        }
        let cipher = Aes256Gcm::new_from_slice(data_key.as_slice())
            .map_err(|_| McpFederationError::CredentialUnopenable)?;
        // Bound to the tenant and the server it was sealed for, so an envelope
        // lifted from one row cannot be replayed against another.
        let aad = format!("{tenant_id}:{}", server.server_id);
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| McpFederationError::CredentialUnopenable)?,
        );
        let credential = std::str::from_utf8(plaintext.as_slice())
            .map_err(|_| McpFederationError::CredentialUnopenable)?;
        Ok(Some(Zeroizing::new(credential.to_owned())))
    }
}

/// The registry already refuses anything but HTTPS or loopback HTTP. Checked
/// again here because this is the process that actually opens the connection,
/// and it is the last place a bad endpoint can still be stopped.
fn require_permitted_endpoint(endpoint: &str) -> Result<(), McpFederationError> {
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|_| McpFederationError::EndpointNotPermitted(endpoint.to_owned()))?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    let permitted = parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback);
    if !permitted || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(McpFederationError::EndpointNotPermitted(endpoint.to_owned()));
    }
    Ok(())
}

fn catalog_digest(tools: &[McpTool]) -> String {
    let mut hasher = Sha256::new();
    for tool in tools {
        hasher.update(tool.qualified_name.as_bytes());
        hasher.update([0]);
        hasher.update(tool.input_schema_json.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_http_endpoint_off_loopback_is_refused() {
        assert!(require_permitted_endpoint("http://mcp.example.com/rpc").is_err());
        assert!(require_permitted_endpoint("https://mcp.example.com/rpc").is_ok());
        assert!(require_permitted_endpoint("http://127.0.0.1:8931/rpc").is_ok());
    }

    #[test]
    fn credentials_in_the_endpoint_are_refused() {
        assert!(require_permitted_endpoint("https://user:pass@mcp.example.com/rpc").is_err());
    }

    /// Two catalogs differing only in a tool's input schema must not share a
    /// digest, or a server could change what a tool accepts inside a Run that
    /// froze the old shape.
    #[test]
    fn the_digest_covers_input_schemas_not_just_names() {
        let one = vec![McpTool {
            qualified_name: "mcp:search/web".into(),
            description: "first".into(),
            input_schema_json: r#"{"type":"object"}"#.into(),
        }];
        let two = vec![McpTool {
            qualified_name: "mcp:search/web".into(),
            description: "second".into(),
            input_schema_json: r#"{"type":"string"}"#.into(),
        }];
        assert_ne!(catalog_digest(&one), catalog_digest(&two));
    }

    /// The description is deliberately outside the digest: servers reword them,
    /// and a Run failing because a sentence changed would be noise.
    #[test]
    fn the_digest_ignores_a_reworded_description() {
        let one = vec![McpTool {
            qualified_name: "mcp:search/web".into(),
            description: "search the web".into(),
            input_schema_json: "{}".into(),
        }];
        let two = vec![McpTool {
            qualified_name: "mcp:search/web".into(),
            description: "Search the web.".into(),
            input_schema_json: "{}".into(),
        }];
        assert_eq!(catalog_digest(&one), catalog_digest(&two));
    }
}
