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
use crate::mcp_oauth::{McpOAuthBinding, McpOAuthCoordinator, McpOAuthError};
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use agent_protocol::{
    McpClientCapability, McpElicitationRequest, McpInputAction, McpInputResponse,
    McpPromptArgument, McpPromptDescriptor, McpPromptMessage, McpPromptPage, McpPromptResult,
    McpProtocolRevision, McpResourceContent, McpResourceDescriptor, McpResourcePage,
    McpResourceReadResult, McpResourceTemplateDescriptor, McpResourceTemplatePage,
    McpServerCapability,
};
use base64::Engine;
use chrono::Utc;
use futures_util::StreamExt;
use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

const ENVELOPE_ALGORITHM: &str = "RSA-OAEP-256+A256GCM";
const LEGACY_MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MODERN_MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// A federated result is untrusted third-party content headed for the model's
/// context. Unbounded, one server could exhaust the Run's context or the
/// gateway's memory by answering a one-word question with a gigabyte.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// A server advertising thousands of tools would push everything else out of
/// the prompt. Sixty-four is already more than a Skill can plausibly declare and
/// reason about, so this bounds damage rather than expressing a target.
const MAX_TOOLS: usize = 64;
const MAX_DIRECTORY_ENTRIES: usize = 64;
const MAX_RESOURCE_CONTENTS: usize = 16;
const MAX_PROMPT_ARGUMENTS: usize = 32;
const MAX_PROMPT_MESSAGES: usize = 32;
const MAX_CURSOR_BYTES: usize = 2 * 1024;
const MAX_URI_BYTES: usize = 4 * 1024;
const MAX_NAME_BYTES: usize = 128;
const MAX_TITLE_BYTES: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
const MAX_MIME_TYPE_BYTES: usize = 256;
const MAX_PROMPT_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_RESOURCE_BLOB_BYTES: usize = 192 * 1024;

#[derive(Clone, Debug)]
pub struct McpServerRef {
    pub server_id: Uuid,
    /// Namespace in qualified tool names: `mcp:<name>/<tool>`.
    pub name: String,
    pub endpoint: String,
    /// Sealed. JSON of the envelope, opened here and nowhere else.
    pub credential_envelope_json: String,
    /// Stable handle resolved only inside the credential domain.
    pub oauth_credential_id: Option<Uuid>,
    pub protocol_revision: McpProtocolRevision,
    pub client_capabilities: BTreeSet<McpClientCapability>,
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
    /// Server-advertised surfaces. These are discovery facts, never authority.
    pub capabilities: BTreeSet<McpServerCapability>,
    /// Digest of the complete directory as discovered. A Run freezes this at
    /// start and presents it on every call; a server whose Tool schema or
    /// advertised surface changed underneath does not get to change what the
    /// Run may do.
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpToolResult {
    pub content_json: String,
    /// The server reported the call failed. Distinct from a transport failure:
    /// the model should see this, and nothing should be retried.
    pub is_error: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct McpRoundTripRequired {
    pub round: u8,
    pub request_state: String,
    pub requests: BTreeMap<String, McpElicitationRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct McpRoundTripContinuation {
    /// The round being sent. First continuation is round 2.
    pub round: u8,
    pub request_state: String,
    pub responses: BTreeMap<String, McpInputResponse>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpToolCallOutcome {
    Complete(McpToolResult),
    InputRequired(McpRoundTripRequired),
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpProgressNotification {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

/// Per-request MCP lifecycle wiring. The progress queue is bounded by its
/// caller; `try_send` prevents a remote server from applying backpressure to
/// the Tool execution path.
#[derive(Clone, Debug)]
pub struct McpCallLifecycle {
    pub cancellation: CancellationToken,
    pub progress: mpsc::Sender<McpProgressNotification>,
    pub progress_token: String,
    pub cancellation_reason: String,
}

/// Converts a protocol-level `tools/list` result into the one canonical catalog
/// used by HTTP, stdio and every future transport backend.
pub fn catalog_from_list_result(
    server_name: &str,
    result: &serde_json::Value,
    capabilities: BTreeSet<McpServerCapability>,
) -> Result<McpCatalog, McpFederationError> {
    if !capabilities.contains(&McpServerCapability::Tools) {
        return Err(McpFederationError::Protocol(
            "tools/list was used without an advertised tools capability".into(),
        ));
    }
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
        if name.is_empty() || name.contains('/') || name.contains(':') || name.len() > 128 {
            return Err(McpFederationError::Protocol(format!(
                "tool name {name:?} cannot be qualified unambiguously"
            )));
        }
        tools.push(McpTool {
            qualified_name: format!("mcp:{server_name}/{name}"),
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
    let digest = catalog_digest(&capabilities, &tools);
    Ok(McpCatalog {
        tools,
        capabilities,
        digest,
    })
}

#[must_use]
pub fn empty_catalog_for_capabilities(capabilities: BTreeSet<McpServerCapability>) -> McpCatalog {
    let digest = catalog_digest(&capabilities, &[]);
    McpCatalog {
        tools: Vec::new(),
        capabilities,
        digest,
    }
}

/// Converts a protocol-level `tools/call` result without interpreting the
/// untrusted content headed back to the model.
#[must_use]
pub fn tool_result_from_call_result(result: &serde_json::Value) -> McpToolResult {
    McpToolResult {
        is_error: result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        content_json: result
            .get("content")
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "[]".to_owned()),
    }
}

pub fn resource_page_from_list_result(
    result: &serde_json::Value,
) -> Result<McpResourcePage, McpFederationError> {
    let listed = result
        .get("resources")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            McpFederationError::Protocol("resources/list returned no resources".into())
        })?;
    if listed.len() > MAX_DIRECTORY_ENTRIES {
        return Err(McpFederationError::Protocol(format!(
            "resources/list returned more than {MAX_DIRECTORY_ENTRIES} entries"
        )));
    }
    let resources = listed
        .iter()
        .map(resource_descriptor_from_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpResourcePage {
        resources,
        next_cursor: opaque_cursor(result.get("nextCursor"))?,
    })
}

pub fn resource_read_from_result(
    result: &serde_json::Value,
) -> Result<McpResourceReadResult, McpFederationError> {
    let contents = result
        .get("contents")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            McpFederationError::Protocol("resources/read returned no contents".into())
        })?;
    if contents.is_empty() || contents.len() > MAX_RESOURCE_CONTENTS {
        return Err(McpFederationError::Protocol(format!(
            "resources/read must return between 1 and {MAX_RESOURCE_CONTENTS} content entries"
        )));
    }
    let contents = contents
        .iter()
        .map(resource_content_from_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpResourceReadResult { contents })
}

pub fn resource_template_page_from_list_result(
    result: &serde_json::Value,
) -> Result<McpResourceTemplatePage, McpFederationError> {
    let listed = result
        .get("resourceTemplates")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            McpFederationError::Protocol(
                "resources/templates/list returned no resourceTemplates".into(),
            )
        })?;
    if listed.len() > MAX_DIRECTORY_ENTRIES {
        return Err(McpFederationError::Protocol(format!(
            "resources/templates/list returned more than {MAX_DIRECTORY_ENTRIES} entries"
        )));
    }
    let resource_templates = listed
        .iter()
        .map(|value| {
            Ok(McpResourceTemplateDescriptor {
                uri_template: required_bounded_string(value, "uriTemplate", MAX_URI_BYTES)?,
                name: required_bounded_string(value, "name", MAX_NAME_BYTES)?,
                title: optional_bounded_string(value, "title", MAX_TITLE_BYTES)?,
                description: optional_bounded_string(value, "description", MAX_DESCRIPTION_BYTES)?,
                mime_type: optional_bounded_string(value, "mimeType", MAX_MIME_TYPE_BYTES)?,
            })
        })
        .collect::<Result<Vec<_>, McpFederationError>>()?;
    Ok(McpResourceTemplatePage {
        resource_templates,
        next_cursor: opaque_cursor(result.get("nextCursor"))?,
    })
}

pub fn prompt_page_from_list_result(
    result: &serde_json::Value,
) -> Result<McpPromptPage, McpFederationError> {
    let listed = result
        .get("prompts")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| McpFederationError::Protocol("prompts/list returned no prompts".into()))?;
    if listed.len() > MAX_DIRECTORY_ENTRIES {
        return Err(McpFederationError::Protocol(format!(
            "prompts/list returned more than {MAX_DIRECTORY_ENTRIES} entries"
        )));
    }
    let prompts = listed
        .iter()
        .map(prompt_descriptor_from_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpPromptPage {
        prompts,
        next_cursor: opaque_cursor(result.get("nextCursor"))?,
    })
}

pub fn prompt_result_from_get_result(
    result: &serde_json::Value,
) -> Result<McpPromptResult, McpFederationError> {
    let messages = result
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| McpFederationError::Protocol("prompts/get returned no messages".into()))?;
    if messages.is_empty() || messages.len() > MAX_PROMPT_MESSAGES {
        return Err(McpFederationError::Protocol(format!(
            "prompts/get must return between 1 and {MAX_PROMPT_MESSAGES} messages"
        )));
    }
    let messages = messages
        .iter()
        .map(|message| {
            let role = required_bounded_string(message, "role", 16)?;
            if role != "user" && role != "assistant" {
                return Err(McpFederationError::Protocol(
                    "prompt message role is unsupported".into(),
                ));
            }
            let content = message.get("content").cloned().ok_or_else(|| {
                McpFederationError::Protocol("prompt message has no content".into())
            })?;
            if !content.is_object()
                || serde_json::to_vec(&content)
                    .map_or(true, |encoded| encoded.len() > MAX_RESPONSE_BYTES)
            {
                return Err(McpFederationError::Protocol(
                    "prompt message content is malformed or unbounded".into(),
                ));
            }
            Ok(McpPromptMessage { role, content })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(McpPromptResult {
        description: optional_bounded_string(result, "description", MAX_DESCRIPTION_BYTES)?,
        messages,
    })
}

fn resource_descriptor_from_value(
    value: &serde_json::Value,
) -> Result<McpResourceDescriptor, McpFederationError> {
    let size = match value.get("size") {
        None | Some(serde_json::Value::Null) => None,
        Some(size) => Some(size.as_u64().ok_or_else(|| {
            McpFederationError::Protocol("resource size is not a non-negative integer".into())
        })?),
    };
    Ok(McpResourceDescriptor {
        uri: required_bounded_uri(value, "uri")?,
        name: required_bounded_string(value, "name", MAX_NAME_BYTES)?,
        title: optional_bounded_string(value, "title", MAX_TITLE_BYTES)?,
        description: optional_bounded_string(value, "description", MAX_DESCRIPTION_BYTES)?,
        mime_type: optional_bounded_string(value, "mimeType", MAX_MIME_TYPE_BYTES)?,
        size,
    })
}

fn resource_content_from_value(
    value: &serde_json::Value,
) -> Result<McpResourceContent, McpFederationError> {
    let uri = required_bounded_uri(value, "uri")?;
    let mime_type = optional_bounded_string(value, "mimeType", MAX_MIME_TYPE_BYTES)?;
    match (
        value.get("text").and_then(serde_json::Value::as_str),
        value.get("blob").and_then(serde_json::Value::as_str),
    ) {
        (Some(text), None) if text.len() <= MAX_RESPONSE_BYTES => Ok(McpResourceContent::Text {
            uri,
            mime_type,
            text: text.to_owned(),
        }),
        (None, Some(blob)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .map_err(|_| McpFederationError::Protocol("resource blob is not base64".into()))?;
            if bytes.len() > MAX_RESOURCE_BLOB_BYTES {
                return Err(McpFederationError::Protocol(
                    "resource blob exceeds the decoded byte limit".into(),
                ));
            }
            Ok(McpResourceContent::Blob {
                uri,
                mime_type,
                bytes,
            })
        }
        _ => Err(McpFederationError::Protocol(
            "resource content must contain exactly one bounded text or blob body".into(),
        )),
    }
}

fn prompt_descriptor_from_value(
    value: &serde_json::Value,
) -> Result<McpPromptDescriptor, McpFederationError> {
    let arguments = match value.get("arguments") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(arguments) => {
            let arguments = arguments.as_array().ok_or_else(|| {
                McpFederationError::Protocol("prompt arguments are not an array".into())
            })?;
            if arguments.len() > MAX_PROMPT_ARGUMENTS {
                return Err(McpFederationError::Protocol(format!(
                    "prompt has more than {MAX_PROMPT_ARGUMENTS} arguments"
                )));
            }
            arguments
                .iter()
                .map(|argument| {
                    Ok(McpPromptArgument {
                        name: required_bounded_string(argument, "name", MAX_NAME_BYTES)?,
                        description: optional_bounded_string(
                            argument,
                            "description",
                            MAX_DESCRIPTION_BYTES,
                        )?,
                        required: argument.get("required").map_or(Ok(false), |value| {
                            value.as_bool().ok_or_else(|| {
                                McpFederationError::Protocol(
                                    "prompt argument required flag is not boolean".into(),
                                )
                            })
                        })?,
                    })
                })
                .collect::<Result<Vec<_>, McpFederationError>>()?
        }
    };
    Ok(McpPromptDescriptor {
        name: required_bounded_string(value, "name", MAX_NAME_BYTES)?,
        title: optional_bounded_string(value, "title", MAX_TITLE_BYTES)?,
        description: optional_bounded_string(value, "description", MAX_DESCRIPTION_BYTES)?,
        arguments,
    })
}

fn validate_cursor(cursor: Option<&str>) -> Result<(), McpFederationError> {
    if cursor.is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES) {
        return Err(McpFederationError::Protocol(
            "MCP pagination cursor is empty or unbounded".into(),
        ));
    }
    Ok(())
}

fn opaque_cursor(value: Option<&serde_json::Value>) -> Result<Option<String>, McpFederationError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let cursor = value.as_str().ok_or_else(|| {
                McpFederationError::Protocol("MCP next cursor is not a string".into())
            })?;
            validate_cursor(Some(cursor))?;
            Ok(Some(cursor.to_owned()))
        }
    }
}

fn required_bounded_uri(
    value: &serde_json::Value,
    field: &str,
) -> Result<String, McpFederationError> {
    let uri = required_bounded_string(value, field, MAX_URI_BYTES)?;
    if uri.chars().any(char::is_control) {
        return Err(McpFederationError::Protocol(
            "resource URI contains control characters".into(),
        ));
    }
    Ok(uri)
}

fn required_bounded_string(
    value: &serde_json::Value,
    field: &str,
    max: usize,
) -> Result<String, McpFederationError> {
    let text = value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| McpFederationError::Protocol(format!("MCP {field} is missing")))?;
    if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(McpFederationError::Protocol(format!(
            "MCP {field} is empty, unbounded, or contains controls"
        )));
    }
    Ok(text.to_owned())
}

fn optional_bounded_string(
    value: &serde_json::Value,
    field: &str,
    max: usize,
) -> Result<Option<String>, McpFederationError> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let text = value.as_str().ok_or_else(|| {
                McpFederationError::Protocol(format!("MCP {field} is not a string"))
            })?;
            if text.len() > max {
                return Err(McpFederationError::Protocol(format!(
                    "MCP {field} is unbounded"
                )));
            }
            Ok(Some(text.to_owned()))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpFederationError {
    #[error("mcp server credential envelope could not be opened")]
    CredentialUnopenable,
    #[error("mcp server OAuth authorization is required")]
    AuthorizationRequired,
    #[error("mcp OAuth credential domain is unavailable")]
    CredentialDomainUnavailable,
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
    #[error("mcp request was cancelled")]
    Cancelled,
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
    id: Option<serde_json::Value>,
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
    private_key: Option<RsaPrivateKey>,
    key_id: Option<String>,
    /// Reused only for IP-literal endpoints, where there is no DNS answer to
    /// change between policy validation and connect.
    http: reqwest::Client,
    request_timeout: Duration,
    /// Whether a tenant may register a loopback endpoint. Off unless the
    /// deployment says otherwise.
    loopback_permitted: bool,
    oauth_coordinator: Option<Arc<McpOAuthCoordinator>>,
    modern_request_sequence: Arc<AtomicU64>,
}

impl McpFederationClient {
    /// The key id is derived from the key rather than supplied.
    ///
    /// Taking it as a parameter is a way to configure a mismatch that shows up
    /// only as envelopes that will not open, and the model credential path
    /// already derives it the same way. The 3072-bit floor matches that path
    /// too: a weaker key must not be acceptable here just because this is the
    /// newer code.
    pub fn from_pkcs8_pem(
        pem: &str,
        request_timeout: Duration,
        loopback_permitted: bool,
    ) -> Result<Self, McpFederationError> {
        let private_key = RsaPrivateKey::from_pkcs8_pem(pem)
            .map_err(|_| McpFederationError::CredentialUnopenable)?;
        if private_key.n().bits() < 3072 || request_timeout.is_zero() {
            return Err(McpFederationError::CredentialUnopenable);
        }
        let public_der = RsaPublicKey::from(&private_key)
            .to_public_key_der()
            .map_err(|_| McpFederationError::CredentialUnopenable)?;
        let key_id = hex::encode(Sha256::digest(public_der.as_ref()));
        let http = build_http_client(request_timeout)?;
        Ok(Self {
            private_key: Some(private_key),
            key_id: Some(key_id),
            http,
            request_timeout,
            loopback_permitted,
            oauth_coordinator: None,
            modern_request_sequence: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Creates an in-process client that can reach only credential-free MCP
    /// registrations. Local Runtime mode has no separate credential domain to
    /// unseal at, so accepting an envelope here would collapse the cloud trust
    /// boundary into the Agent process.
    pub fn for_open_servers(
        request_timeout: Duration,
        loopback_permitted: bool,
    ) -> Result<Self, McpFederationError> {
        if request_timeout.is_zero() {
            return Err(McpFederationError::CredentialUnopenable);
        }
        Ok(Self {
            private_key: None,
            key_id: None,
            http: build_http_client(request_timeout)?,
            request_timeout,
            loopback_permitted,
            oauth_coordinator: None,
            modern_request_sequence: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Installs the credential-domain OAuth coordinator. The coordinator stays
    /// in the Gateway process; callers still carry only `oauth_credential_id`.
    #[must_use]
    pub fn with_oauth_coordinator(mut self, coordinator: Arc<McpOAuthCoordinator>) -> Self {
        self.oauth_coordinator = Some(coordinator);
        self
    }

    /// Discovers what the server offers, qualified and digested.
    pub async fn list_tools(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
    ) -> Result<McpCatalog, McpFederationError> {
        let http = self.http_for_server(server)?;
        let credential = self.resolve_credential(tenant_id, server).await?;
        if server.protocol_revision == McpProtocolRevision::V2026_07_28 {
            let discover = self
                .call_modern_json_rpc(
                    &http,
                    server,
                    credential.as_deref().map(String::as_str),
                    "server/discover",
                    serde_json::json!({}),
                )
                .await?;
            let capabilities = validate_modern_discovery(&discover)?;
            if !capabilities.contains(&McpServerCapability::Tools) {
                return Ok(empty_catalog_for_capabilities(capabilities));
            }
            let result = self
                .call_modern_json_rpc(
                    &http,
                    server,
                    credential.as_deref().map(String::as_str),
                    "tools/list",
                    serde_json::json!({}),
                )
                .await?;
            validate_complete_result(&result, "tools/list")?;
            return catalog_from_list_result(&server.name, &result, capabilities);
        }
        let (session, capabilities) = self
            .initialize(&http, server, credential.as_deref().map(String::as_str))
            .await?;
        if !capabilities.contains(&McpServerCapability::Tools) {
            return Ok(empty_catalog_for_capabilities(capabilities));
        }
        let result = self
            .call_json_rpc(
                &http,
                server,
                credential.as_deref().map(String::as_str),
                session.as_deref(),
                "tools/list",
                serde_json::json!({}),
            )
            .await?
            .0;
        catalog_from_list_result(&server.name, &result, capabilities)
    }

    pub async fn list_resources(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        frozen_catalog_digest: &str,
        cursor: Option<&str>,
    ) -> Result<McpResourcePage, McpFederationError> {
        validate_cursor(cursor)?;
        let mut params = serde_json::Map::new();
        if let Some(cursor) = cursor {
            params.insert(
                "cursor".into(),
                serde_json::Value::String(cursor.to_owned()),
            );
        }
        let result = self
            .read_surface(
                tenant_id,
                server,
                frozen_catalog_digest,
                McpServerCapability::Resources,
                "resources/list",
                serde_json::Value::Object(params),
            )
            .await?;
        resource_page_from_list_result(&result)
    }

    pub async fn read_resource(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        frozen_catalog_digest: &str,
        uri: &str,
    ) -> Result<McpResourceReadResult, McpFederationError> {
        if uri.is_empty() || uri.len() > MAX_URI_BYTES || uri.chars().any(char::is_control) {
            return Err(McpFederationError::Protocol(
                "resource URI is empty or unbounded".into(),
            ));
        }
        let result = self
            .read_surface(
                tenant_id,
                server,
                frozen_catalog_digest,
                McpServerCapability::Resources,
                "resources/read",
                serde_json::json!({"uri": uri}),
            )
            .await?;
        resource_read_from_result(&result)
    }

    pub async fn list_resource_templates(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        frozen_catalog_digest: &str,
        cursor: Option<&str>,
    ) -> Result<McpResourceTemplatePage, McpFederationError> {
        validate_cursor(cursor)?;
        let mut params = serde_json::Map::new();
        if let Some(cursor) = cursor {
            params.insert(
                "cursor".into(),
                serde_json::Value::String(cursor.to_owned()),
            );
        }
        let result = self
            .read_surface(
                tenant_id,
                server,
                frozen_catalog_digest,
                McpServerCapability::Resources,
                "resources/templates/list",
                serde_json::Value::Object(params),
            )
            .await?;
        resource_template_page_from_list_result(&result)
    }

    pub async fn list_prompts(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        frozen_catalog_digest: &str,
        cursor: Option<&str>,
    ) -> Result<McpPromptPage, McpFederationError> {
        validate_cursor(cursor)?;
        let mut params = serde_json::Map::new();
        if let Some(cursor) = cursor {
            params.insert(
                "cursor".into(),
                serde_json::Value::String(cursor.to_owned()),
            );
        }
        let result = self
            .read_surface(
                tenant_id,
                server,
                frozen_catalog_digest,
                McpServerCapability::Prompts,
                "prompts/list",
                serde_json::Value::Object(params),
            )
            .await?;
        prompt_page_from_list_result(&result)
    }

    pub async fn get_prompt(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        frozen_catalog_digest: &str,
        name: &str,
        arguments: Option<&serde_json::Value>,
    ) -> Result<McpPromptResult, McpFederationError> {
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(McpFederationError::Protocol(
                "prompt name is empty or unbounded".into(),
            ));
        }
        let mut params = serde_json::Map::from_iter([(
            "name".into(),
            serde_json::Value::String(name.to_owned()),
        )]);
        if let Some(arguments) = arguments {
            let object = arguments.as_object().ok_or_else(|| {
                McpFederationError::Protocol("prompt arguments must be an object".into())
            })?;
            if object.len() > MAX_PROMPT_ARGUMENTS
                || object.values().any(|value| !value.is_string())
                || serde_json::to_vec(arguments)
                    .map_or(true, |encoded| encoded.len() > MAX_PROMPT_ARGUMENT_BYTES)
            {
                return Err(McpFederationError::Protocol(
                    "prompt arguments are malformed or unbounded".into(),
                ));
            }
            params.insert("arguments".into(), arguments.clone());
        }
        let result = self
            .read_surface(
                tenant_id,
                server,
                frozen_catalog_digest,
                McpServerCapability::Prompts,
                "prompts/get",
                serde_json::Value::Object(params),
            )
            .await?;
        prompt_result_from_get_result(&result)
    }

    async fn read_surface(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        frozen_catalog_digest: &str,
        required_capability: McpServerCapability,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpFederationError> {
        let catalog = self.list_tools(tenant_id, server).await?;
        if catalog.digest != frozen_catalog_digest
            || !catalog.capabilities.contains(&required_capability)
        {
            return Err(McpFederationError::CatalogChanged);
        }

        let http = self.http_for_server(server)?;
        let credential = self.resolve_credential(tenant_id, server).await?;
        if server.protocol_revision == McpProtocolRevision::V2026_07_28 {
            let discovery = self
                .call_modern_json_rpc(
                    &http,
                    server,
                    credential.as_deref().map(String::as_str),
                    "server/discover",
                    serde_json::json!({}),
                )
                .await?;
            let capabilities = validate_modern_discovery(&discovery)?;
            if !capabilities.contains(&required_capability) {
                return Err(McpFederationError::CatalogChanged);
            }
            let result = self
                .call_modern_json_rpc(
                    &http,
                    server,
                    credential.as_deref().map(String::as_str),
                    method,
                    params,
                )
                .await?;
            validate_complete_result(&result, method)?;
            return Ok(result);
        }

        let (session, capabilities) = self
            .initialize(&http, server, credential.as_deref().map(String::as_str))
            .await?;
        if !capabilities.contains(&required_capability) {
            return Err(McpFederationError::CatalogChanged);
        }
        self.call_json_rpc(
            &http,
            server,
            credential.as_deref().map(String::as_str),
            session.as_deref(),
            method,
            params,
        )
        .await
        .map(|(result, _)| result)
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
        match self
            .call_tool_round(
                tenant_id,
                server,
                qualified_name,
                arguments_json,
                frozen_catalog_digest,
                None,
            )
            .await?
        {
            McpToolCallOutcome::Complete(result) => Ok(result),
            McpToolCallOutcome::InputRequired(_) => Err(McpFederationError::Protocol(
                "MCP Tool requires user input but this caller has no MRTR continuation path".into(),
            )),
        }
    }

    /// Executes one stateless MCP round. `input_required` is returned as data,
    /// never hidden behind a held-open callback, so the caller can persist it
    /// before asking a user and resume after process replacement.
    pub async fn call_tool_round(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        qualified_name: &str,
        arguments_json: &str,
        frozen_catalog_digest: &str,
        continuation: Option<&McpRoundTripContinuation>,
    ) -> Result<McpToolCallOutcome, McpFederationError> {
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
            .ok_or_else(|| McpFederationError::ToolNotInFrozenCatalog(qualified_name.to_owned()))?;
        let arguments: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
        let http = self.http_for_server(server)?;
        let credential = self.resolve_credential(tenant_id, server).await?;
        if server.protocol_revision == McpProtocolRevision::V2026_07_28 {
            let round = continuation.map_or(1, |continuation| continuation.round);
            if !(1..=10).contains(&round) {
                return Err(McpFederationError::Protocol(
                    "MCP MRTR round must be between 1 and 10".into(),
                ));
            }
            let mut params = serde_json::json!({ "name": bare, "arguments": arguments });
            if let Some(continuation) = continuation {
                if continuation.request_state.is_empty()
                    || continuation.request_state.len() > 64 * 1024
                    || continuation.responses.is_empty()
                    || continuation.responses.len() > 8
                {
                    return Err(McpFederationError::Protocol(
                        "MCP MRTR continuation is malformed or unbounded".into(),
                    ));
                }
                params["requestState"] =
                    serde_json::Value::String(continuation.request_state.clone());
                params["inputResponses"] = mrtr_responses_value(&continuation.responses)?;
            }
            let result = self
                .call_modern_json_rpc(
                    &http,
                    server,
                    credential.as_deref().map(String::as_str),
                    "tools/call",
                    params,
                )
                .await?;
            return match result.get("resultType").and_then(serde_json::Value::as_str) {
                Some("complete") => Ok(McpToolCallOutcome::Complete(tool_result_from_call_result(
                    &result,
                ))),
                Some("input_required") => {
                    if round == 10 {
                        return Err(McpFederationError::Protocol(
                            "MCP Tool exceeded the 10-round MRTR limit".into(),
                        ));
                    }
                    Ok(McpToolCallOutcome::InputRequired(
                        parse_modern_input_required(&server.client_capabilities, &result, round)?,
                    ))
                }
                other => Err(McpFederationError::Protocol(format!(
                    "tools/call returned unsupported resultType {other:?}"
                ))),
            };
        }
        if continuation.is_some() {
            return Err(McpFederationError::Protocol(
                "MCP 2025-06-18 does not support stateless MRTR continuation".into(),
            ));
        }
        // A fresh session per call. Reusing one across calls would mean holding
        // server-side state whose lifetime we do not control, and a call that
        // silently ran in an expired session is worse than one extra handshake.
        let (session, capabilities) = self
            .initialize(&http, server, credential.as_deref().map(String::as_str))
            .await?;
        if !capabilities.contains(&McpServerCapability::Tools) {
            return Err(McpFederationError::CatalogChanged);
        }
        let result = self
            .call_json_rpc(
                &http,
                server,
                credential.as_deref().map(String::as_str),
                session.as_deref(),
                "tools/call",
                serde_json::json!({ "name": bare, "arguments": arguments }),
            )
            .await?
            .0;
        Ok(McpToolCallOutcome::Complete(tool_result_from_call_result(
            &result,
        )))
    }

    /// Calls a Tool with the MCP request lifecycle made observable. Only the
    /// actual `tools/call` receives a progress token; discovery remains a safe,
    /// separately bounded operation and is never confused with side effects.
    pub async fn call_tool_with_lifecycle(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
        qualified_name: &str,
        arguments_json: &str,
        frozen_catalog_digest: &str,
        lifecycle: &McpCallLifecycle,
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
        if lifecycle.progress_token.is_empty() {
            return Err(McpFederationError::Protocol(
                "MCP progress token must not be empty".into(),
            ));
        }
        let bare = qualified_name
            .rsplit_once('/')
            .map(|(_, tool)| tool)
            .ok_or_else(|| McpFederationError::ToolNotInFrozenCatalog(qualified_name.to_owned()))?;
        let arguments: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
        let http = self.http_for_server(server)?;
        let credential = self.resolve_credential(tenant_id, server).await?;
        let (session, capabilities) = self
            .initialize(&http, server, credential.as_deref().map(String::as_str))
            .await?;
        if !capabilities.contains(&McpServerCapability::Tools) {
            return Err(McpFederationError::CatalogChanged);
        }
        let request_id = &lifecycle.progress_token;
        let result = self
            .call_json_rpc_with_lifecycle(
                &http,
                server,
                credential.as_deref().map(String::as_str),
                session.as_deref(),
                request_id,
                serde_json::json!({
                    "name": bare,
                    "arguments": arguments,
                    "_meta": {"progressToken": lifecycle.progress_token}
                }),
                lifecycle,
            )
            .await?;
        Ok(tool_result_from_call_result(&result))
    }

    /// Opens a session and completes the handshake.
    ///
    /// Returns the server's `Mcp-Session-Id` when it issues one. Streamable HTTP
    /// servers that keep session state refuse every later request without it --
    /// the reference implementation answers 400 -- so this is not optional for
    /// anything beyond a stateless server.
    async fn initialize(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
    ) -> Result<(Option<String>, BTreeSet<McpServerCapability>), McpFederationError> {
        let (initialize_result, session) = self
            .call_json_rpc(
                http,
                server,
                credential,
                None,
                "initialize",
                serde_json::json!({
                    "protocolVersion": LEGACY_MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "agent-runtime-platform", "version": "1" }
                }),
            )
            .await?;
        let capabilities = validate_initialize_result(&initialize_result)?;
        // The spec has the client confirm initialization. It is a notification,
        // so there is no result to wait for and a server that ignores it is
        // still conformant; failing the whole discovery over it would be worse
        // than sending it and moving on.
        self.notify(http, server, credential, session.as_deref())
            .await?;
        Ok((session, capabilities))
    }

    async fn notify(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
    ) -> Result<(), McpFederationError> {
        let request = self
            .request_builder(http, server, credential, session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }));
        let response = request
            .send()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        if !response.status().is_success() {
            return Err(McpFederationError::Unreachable(format!(
                "server answered HTTP {} to the initialized notification",
                response.status().as_u16()
            )));
        }
        Ok(())
    }

    async fn call_modern_json_rpc(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        method: &str,
        mut params: serde_json::Value,
    ) -> Result<serde_json::Value, McpFederationError> {
        let params_object = params.as_object_mut().ok_or_else(|| {
            McpFederationError::Protocol("modern MCP params must be an object".into())
        })?;
        attach_modern_request_metadata(&server.client_capabilities, params_object)?;

        let request_id = self
            .modern_request_sequence
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let mut request = self
            .request_builder(http, server, credential, None)
            .header("mcp-method", method);
        if method == "tools/call"
            && let Some(name) = params.get("name").and_then(serde_json::Value::as_str)
        {
            request = request.header("mcp-name", name);
        }
        let request_id_value = serde_json::json!(request_id);
        let response = request
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id_value,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        let status = response.status();
        let sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if response
            .content_length()
            .is_some_and(|length| length as usize > MAX_RESPONSE_BYTES)
        {
            return Err(McpFederationError::ResponseTooLarge);
        }
        if !status.is_success() {
            return Err(McpFederationError::Unreachable(format!(
                "server answered HTTP {}",
                status.as_u16()
            )));
        }
        let decoded = if sse {
            self.read_basic_event_stream(
                http,
                server,
                credential,
                None,
                &request_id_value,
                response,
            )
            .await?
        } else {
            let body = response
                .bytes()
                .await
                .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(McpFederationError::ResponseTooLarge);
            }
            let frame: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
            if is_server_request(&frame) {
                return Err(McpFederationError::Protocol(
                    "MCP 2026-07-28 server sent a removed reverse request".into(),
                ));
            }
            validate_response_id(&frame, &request_id_value)?;
            serde_json::from_value(frame)
                .map_err(|error| McpFederationError::Protocol(error.to_string()))?
        };
        if let Some(error) = decoded.error {
            return Err(McpFederationError::Protocol(error.message));
        }
        decoded.result.ok_or_else(|| {
            McpFederationError::Protocol("response carried neither result nor error".into())
        })
    }

    /// Headers every Streamable HTTP request needs.
    ///
    /// `Accept` must name both JSON and SSE. Sending only `application/json`
    /// gets a 406 from a conformant server -- the reference implementation
    /// answers "Client must accept both" -- because the transport may reply
    /// either way and the client has to be able to read both.
    fn request_builder(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = http
            .post(&server.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", server.protocol_revision.as_str());
        if let Some(secret) = credential {
            request = request.bearer_auth(secret);
        }
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        request
    }

    fn http_for_server(
        &self,
        server: &McpServerRef,
    ) -> Result<reqwest::Client, McpFederationError> {
        let pinned = resolve_permitted_endpoint(&server.endpoint, self.loopback_permitted)?;
        if let Some(pinned) = pinned {
            reqwest::Client::builder()
                .timeout(self.request_timeout)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                // The connector receives exactly the address set checked above;
                // TLS still authenticates the hostname in the original URL.
                .resolve_to_addrs(&pinned.hostname, &pinned.addresses)
                .build()
                .map_err(|error| McpFederationError::Unreachable(error.to_string()))
        } else {
            Ok(self.http.clone())
        }
    }

    /// Returns the result and the session id the server issued, if any.
    async fn call_json_rpc(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(serde_json::Value, Option<String>), McpFederationError> {
        let request = self
            .request_builder(http, server, credential, session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }));
        let response = request
            .send()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        let status = response.status();
        let issued_session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        // Read the length hint first so an oversized body is refused before it
        // is pulled into memory, not after.
        if response
            .content_length()
            .is_some_and(|length| length as usize > MAX_RESPONSE_BYTES)
        {
            return Err(McpFederationError::ResponseTooLarge);
        }
        if !status.is_success() {
            return Err(McpFederationError::Unreachable(format!(
                "server answered HTTP {}",
                status.as_u16()
            )));
        }
        let decoded = if sse {
            self.read_basic_event_stream(
                http,
                server,
                credential,
                issued_session.as_deref().or(session),
                &serde_json::json!(1),
                response,
            )
            .await?
        } else {
            let body = response
                .bytes()
                .await
                .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
            // A server can omit or lie about Content-Length, and chunked
            // responses have none at all.
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(McpFederationError::ResponseTooLarge);
            }
            let frame: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
            if is_server_request(&frame) {
                return self
                    .reject_unnegotiated_client_request(
                        http,
                        server,
                        credential,
                        issued_session.as_deref().or(session),
                        &frame,
                    )
                    .await;
            }
            validate_response_id(&frame, &serde_json::json!(1))?;
            serde_json::from_value(frame)
                .map_err(|error| McpFederationError::Protocol(error.to_string()))?
        };
        if let Some(error) = decoded.error {
            return Err(McpFederationError::Protocol(error.message));
        }
        let result = decoded.result.ok_or_else(|| {
            McpFederationError::Protocol("response carried neither result nor error".into())
        })?;
        Ok((result, issued_session))
    }

    async fn read_basic_event_stream(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
        expected_id: &serde_json::Value,
        response: reqwest::Response,
    ) -> Result<JsonRpcResponse, McpFederationError> {
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut received = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(McpFederationError::ResponseTooLarge);
            }
            pending.extend_from_slice(&chunk);
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let mut line = pending.drain(..=newline).collect::<Vec<_>>();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let Some(payload) = line.strip_prefix(b"data:") else {
                    continue;
                };
                let frame: serde_json::Value = serde_json::from_slice(trim_ascii(payload))
                    .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
                if is_server_request(&frame) {
                    return self
                        .reject_unnegotiated_client_request(
                            http, server, credential, session, &frame,
                        )
                        .await;
                }
                let decoded: JsonRpcResponse = serde_json::from_value(frame)
                    .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
                if decoded.result.is_some() || decoded.error.is_some() {
                    validate_response_id_from_parts(&decoded, expected_id)?;
                    return Ok(decoded);
                }
            }
        }
        Err(McpFederationError::Protocol(
            "event stream carried no jsonrpc result or error".into(),
        ))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the private transport boundary keeps authority, session and lifecycle inputs explicit"
    )]
    async fn call_json_rpc_with_lifecycle(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
        request_id: &str,
        params: serde_json::Value,
        lifecycle: &McpCallLifecycle,
    ) -> Result<serde_json::Value, McpFederationError> {
        let request = self
            .request_builder(http, server, credential, session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": params,
            }));
        let response = tokio::select! {
            biased;
            () = lifecycle.cancellation.cancelled() => {
                self.send_cancelled(http, server, credential, session, request_id, &lifecycle.cancellation_reason).await;
                return Err(McpFederationError::Cancelled);
            }
            response = request.send() => response
                .map_err(|error| McpFederationError::Unreachable(error.to_string()))?,
        };
        let status = response.status();
        let sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if response
            .content_length()
            .is_some_and(|length| length as usize > MAX_RESPONSE_BYTES)
        {
            return Err(McpFederationError::ResponseTooLarge);
        }
        if !status.is_success() {
            return Err(McpFederationError::Unreachable(format!(
                "server answered HTTP {}",
                status.as_u16()
            )));
        }
        if !sse {
            let body = tokio::select! {
                biased;
                () = lifecycle.cancellation.cancelled() => {
                    self.send_cancelled(http, server, credential, session, request_id, &lifecycle.cancellation_reason).await;
                    return Err(McpFederationError::Cancelled);
                }
                body = response.bytes() => body
                    .map_err(|error| McpFederationError::Unreachable(error.to_string()))?,
            };
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(McpFederationError::ResponseTooLarge);
            }
            let frame: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
            if is_server_request(&frame) {
                return self
                    .reject_unnegotiated_client_request(http, server, credential, session, &frame)
                    .await;
            }
            validate_response_id(&frame, &serde_json::json!(request_id))?;
            let decoded: JsonRpcResponse = serde_json::from_value(frame)
                .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
            if let Some(error) = decoded.error {
                return Err(McpFederationError::Protocol(error.message));
            }
            return decoded.result.ok_or_else(|| {
                McpFederationError::Protocol("response carried neither result nor error".into())
            });
        }

        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut received = 0usize;
        let mut last_progress = None;
        loop {
            let chunk = tokio::select! {
                biased;
                () = lifecycle.cancellation.cancelled() => {
                    self.send_cancelled(http, server, credential, session, request_id, &lifecycle.cancellation_reason).await;
                    return Err(McpFederationError::Cancelled);
                }
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk =
                chunk.map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(McpFederationError::ResponseTooLarge);
            }
            pending.extend_from_slice(&chunk);
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let mut line = pending.drain(..=newline).collect::<Vec<_>>();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let Some(payload) = line.strip_prefix(b"data:") else {
                    continue;
                };
                let frame: serde_json::Value = serde_json::from_slice(trim_ascii(payload))
                    .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
                if is_server_request(&frame) {
                    return self
                        .reject_unnegotiated_client_request(
                            http, server, credential, session, &frame,
                        )
                        .await;
                }
                if frame["method"] == "notifications/progress" {
                    record_progress_frame(&frame, lifecycle, &mut last_progress)?;
                    continue;
                }
                if frame["id"] != request_id {
                    continue;
                }
                if let Some(error) = frame.get("error") {
                    let message = error["message"]
                        .as_str()
                        .unwrap_or("unknown JSON-RPC error");
                    return Err(McpFederationError::Protocol(message.to_owned()));
                }
                if let Some(result) = frame.get("result") {
                    return Ok(result.clone());
                }
            }
        }
        Err(McpFederationError::Protocol(
            "event stream carried no jsonrpc result or error".into(),
        ))
    }

    async fn reject_unnegotiated_client_request<T>(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
        request: &serde_json::Value,
    ) -> Result<T, McpFederationError> {
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap_or("unknown");
        let response = self
            .request_builder(http, server, credential, session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": "client method is not supported"
                }
            }))
            .send()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        if !response.status().is_success() {
            return Err(McpFederationError::Unreachable(format!(
                "server answered HTTP {} to the reverse-request rejection",
                response.status().as_u16()
            )));
        }
        Err(McpFederationError::Protocol(format!(
            "MCP server sent unnegotiated client request {method}"
        )))
    }

    async fn send_cancelled(
        &self,
        http: &reqwest::Client,
        server: &McpServerRef,
        credential: Option<&str>,
        session: Option<&str>,
        request_id: &str,
        reason: &str,
    ) {
        let _ = self
            .request_builder(http, server, credential, session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": request_id, "reason": reason}
            }))
            .send()
            .await;
    }

    async fn resolve_credential(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
    ) -> Result<Option<Zeroizing<String>>, McpFederationError> {
        if let Some(credential_id) = server.oauth_credential_id {
            if !server.credential_envelope_json.trim().is_empty() || credential_id.is_nil() {
                return Err(McpFederationError::CredentialUnopenable);
            }
            let coordinator = self
                .oauth_coordinator
                .as_ref()
                .ok_or(McpFederationError::AuthorizationRequired)?;
            let credential = coordinator
                .resolve_access_token(
                    McpOAuthBinding {
                        tenant_id,
                        server_id: server.server_id,
                        credential_id,
                        endpoint: server.endpoint.clone(),
                    },
                    Utc::now(),
                )
                .await
                .map_err(map_oauth_error)?;
            return Ok(Some(credential.into_access_token()));
        }
        self.open_static_credential(tenant_id, server)
    }

    fn open_static_credential(
        &self,
        tenant_id: Uuid,
        server: &McpServerRef,
    ) -> Result<Option<Zeroizing<String>>, McpFederationError> {
        // An open server is registered without a credential. That is a real
        // configuration, not a missing one, so it is not an error.
        if server.credential_envelope_json.trim().is_empty() {
            return Ok(None);
        }
        let envelope: CredentialEnvelope = serde_json::from_str(&server.credential_envelope_json)
            .map_err(|_| McpFederationError::CredentialUnopenable)?;
        let private_key = self
            .private_key
            .as_ref()
            .ok_or(McpFederationError::CredentialUnopenable)?;
        let key_id = self
            .key_id
            .as_deref()
            .ok_or(McpFederationError::CredentialUnopenable)?;
        if envelope.schema_version != 1
            || envelope.algorithm != ENVELOPE_ALGORITHM
            || envelope.key_id != key_id
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
            private_key
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

fn map_oauth_error(error: McpOAuthError) -> McpFederationError {
    match error {
        McpOAuthError::AuthorizationRequired
        | McpOAuthError::ProviderRejected
        | McpOAuthError::InvalidAuthorizationCallback => McpFederationError::AuthorizationRequired,
        McpOAuthError::InvalidBinding | McpOAuthError::InvalidAuthorizationRequest => {
            McpFederationError::CredentialUnopenable
        }
        McpOAuthError::StoreUnavailable | McpOAuthError::ProviderUnavailable => {
            McpFederationError::CredentialDomainUnavailable
        }
    }
}

/// Adds the MCP 2026 transport-neutral request metadata. HTTP and stdio must
/// advertise the same frozen client authority; transport selection must not
/// silently change what reverse work the server may request.
pub fn attach_modern_request_metadata(
    client_capabilities: &BTreeSet<McpClientCapability>,
    params: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), McpFederationError> {
    let capabilities = if client_capabilities.contains(&McpClientCapability::Elicitation) {
        serde_json::json!({"elicitation": {"form": {}, "url": {}}})
    } else {
        serde_json::json!({})
    };
    let meta = params
        .entry("_meta")
        .or_insert_with(|| serde_json::json!({}));
    let meta = meta
        .as_object_mut()
        .ok_or_else(|| McpFederationError::Protocol("modern MCP _meta must be an object".into()))?;
    meta.insert(
        "io.modelcontextprotocol/protocolVersion".into(),
        serde_json::json!(MODERN_MCP_PROTOCOL_VERSION),
    );
    meta.insert(
        "io.modelcontextprotocol/clientInfo".into(),
        serde_json::json!({"name": "agent-runtime-platform", "version": "1"}),
    );
    meta.insert(
        "io.modelcontextprotocol/clientCapabilities".into(),
        capabilities,
    );
    Ok(())
}

fn validate_modern_discovery(
    result: &serde_json::Value,
) -> Result<BTreeSet<McpServerCapability>, McpFederationError> {
    validate_complete_result(result, "server/discover")?;
    if !result["supportedVersions"]
        .as_array()
        .is_some_and(|versions| {
            versions
                .iter()
                .any(|version| version.as_str() == Some(MODERN_MCP_PROTOCOL_VERSION))
        })
    {
        return Err(McpFederationError::Protocol(
            "MCP server/discover did not advertise 2026-07-28".into(),
        ));
    }
    let capabilities = parse_server_capabilities(&result["capabilities"])?;
    if capabilities.is_empty() {
        return Err(McpFederationError::Protocol(
            "MCP server did not advertise a supported capability".into(),
        ));
    }
    Ok(capabilities)
}

fn validate_complete_result(
    result: &serde_json::Value,
    method: &str,
) -> Result<(), McpFederationError> {
    if result.get("resultType").and_then(serde_json::Value::as_str) != Some("complete") {
        return Err(McpFederationError::Protocol(format!(
            "{method} did not return a complete result"
        )));
    }
    Ok(())
}

pub fn parse_modern_input_required(
    client_capabilities: &BTreeSet<McpClientCapability>,
    result: &serde_json::Value,
    round: u8,
) -> Result<McpRoundTripRequired, McpFederationError> {
    if !client_capabilities.contains(&McpClientCapability::Elicitation) {
        return Err(McpFederationError::Protocol(
            "MCP server requested elicitation without Run authority".into(),
        ));
    }
    let request_state = result["requestState"]
        .as_str()
        .filter(|state| !state.is_empty() && state.len() <= 64 * 1024)
        .ok_or_else(|| {
            McpFederationError::Protocol("MCP input_required has invalid requestState".into())
        })?
        .to_owned();
    let input_requests = result["inputRequests"].as_object().ok_or_else(|| {
        McpFederationError::Protocol("MCP input_required has no inputRequests".into())
    })?;
    if input_requests.is_empty() || input_requests.len() > 8 {
        return Err(McpFederationError::Protocol(
            "MCP input_required request count is outside 1..=8".into(),
        ));
    }
    let mut requests = BTreeMap::new();
    for (key, value) in input_requests {
        if key.is_empty() || key.len() > 128 || requests.contains_key(key) {
            return Err(McpFederationError::Protocol(
                "MCP input request key is invalid".into(),
            ));
        }
        if value["method"].as_str() != Some("elicitation/create") {
            return Err(McpFederationError::Protocol(
                "only elicitation/create MRTR input is authorized".into(),
            ));
        }
        let params = value["params"].as_object().ok_or_else(|| {
            McpFederationError::Protocol("MCP elicitation params are missing".into())
        })?;
        let message = params
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let meta = params.get("_meta").cloned();
        let request = match params
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("form")
        {
            "form" => McpElicitationRequest::Form {
                message,
                requested_schema: params.get("requestedSchema").cloned().ok_or_else(|| {
                    McpFederationError::Protocol("form elicitation has no requestedSchema".into())
                })?,
                meta,
            },
            "url" => McpElicitationRequest::Url {
                message,
                url: params
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                elicitation_id: params
                    .get("elicitationId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                meta,
            },
            mode => {
                return Err(McpFederationError::Protocol(format!(
                    "unsupported MCP elicitation mode {mode}"
                )));
            }
        };
        request
            .validate()
            .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
        requests.insert(key.clone(), request);
    }
    Ok(McpRoundTripRequired {
        round,
        request_state,
        requests,
    })
}

pub fn mrtr_responses_value(
    responses: &BTreeMap<String, McpInputResponse>,
) -> Result<serde_json::Value, McpFederationError> {
    let mut object = serde_json::Map::new();
    for (key, response) in responses {
        if key.is_empty() || key.len() > 128 {
            return Err(McpFederationError::Protocol(
                "MCP input response key is invalid".into(),
            ));
        }
        let mut value = serde_json::Map::from_iter([(
            "action".into(),
            serde_json::Value::String(
                match response.action {
                    McpInputAction::Accept => "accept",
                    McpInputAction::Decline => "decline",
                    McpInputAction::Cancel => "cancel",
                }
                .into(),
            ),
        )]);
        if let Some(content) = &response.content {
            value.insert("content".into(), content.clone());
        }
        if let Some(meta) = &response.meta {
            value.insert("_meta".into(), meta.clone());
        }
        object.insert(key.clone(), serde_json::Value::Object(value));
    }
    let value = serde_json::Value::Object(object);
    if serde_json::to_vec(&value).map_or(true, |encoded| encoded.len() > 128 * 1024) {
        return Err(McpFederationError::Protocol(
            "MCP input responses exceeded 128 KiB".into(),
        ));
    }
    Ok(value)
}

fn validate_initialize_result(
    result: &serde_json::Value,
) -> Result<BTreeSet<McpServerCapability>, McpFederationError> {
    let selected = result["protocolVersion"].as_str().ok_or_else(|| {
        McpFederationError::Protocol("MCP initialize result has no protocolVersion".into())
    })?;
    if selected != LEGACY_MCP_PROTOCOL_VERSION {
        return Err(McpFederationError::Protocol(format!(
            "MCP server selected unsupported protocol version {selected}"
        )));
    }
    let capabilities = parse_server_capabilities(&result["capabilities"])?;
    if capabilities.is_empty() {
        return Err(McpFederationError::Protocol(
            "MCP server did not negotiate a supported capability".into(),
        ));
    }
    Ok(capabilities)
}

fn parse_server_capabilities(
    value: &serde_json::Value,
) -> Result<BTreeSet<McpServerCapability>, McpFederationError> {
    let object = value.as_object().ok_or_else(|| {
        McpFederationError::Protocol("MCP server capabilities must be an object".into())
    })?;
    let mut capabilities = BTreeSet::new();
    for (field, capability) in [
        ("tools", McpServerCapability::Tools),
        ("resources", McpServerCapability::Resources),
        ("prompts", McpServerCapability::Prompts),
    ] {
        if let Some(value) = object.get(field) {
            if !value.is_object() {
                return Err(McpFederationError::Protocol(format!(
                    "MCP server capability {field} must be an object"
                )));
            }
            capabilities.insert(capability);
        }
    }
    Ok(capabilities)
}

fn is_server_request(frame: &serde_json::Value) -> bool {
    frame
        .get("method")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && frame.get("id").is_some()
}

fn validate_response_id(
    frame: &serde_json::Value,
    expected_id: &serde_json::Value,
) -> Result<(), McpFederationError> {
    match frame.get("id") {
        Some(actual) if actual == expected_id => Ok(()),
        Some(actual) => Err(McpFederationError::Protocol(format!(
            "MCP response id {actual} did not match request id {expected_id}"
        ))),
        None => Err(McpFederationError::Protocol(
            "MCP response has no JSON-RPC id".into(),
        )),
    }
}

fn validate_response_id_from_parts(
    decoded: &JsonRpcResponse,
    expected_id: &serde_json::Value,
) -> Result<(), McpFederationError> {
    let actual = decoded
        .id
        .as_ref()
        .ok_or_else(|| McpFederationError::Protocol("MCP response has no JSON-RPC id".into()))?;
    if actual != expected_id {
        return Err(McpFederationError::Protocol(format!(
            "MCP response id {actual} did not match request id {expected_id}"
        )));
    }
    Ok(())
}

fn build_http_client(request_timeout: Duration) -> Result<reqwest::Client, McpFederationError> {
    reqwest::Client::builder()
        .timeout(request_timeout)
        // Environment proxies resolve CONNECT targets outside the pinned
        // resolver below. Proxy support needs an explicit connector that
        // preserves the pin; inheriting HTTP_PROXY would reopen the same
        // DNS-rebinding window.
        .no_proxy()
        // A federated server is the only host reachable for its own calls, so a
        // redirect to somewhere else is exactly what the endpoint field exists
        // to prevent.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| McpFederationError::Unreachable(error.to_string()))
}

#[derive(Debug, Eq, PartialEq)]
struct PinnedDns {
    hostname: String,
    addresses: Vec<SocketAddr>,
}

/// Refuses an endpoint before the scheme is even considered a permission.
///
/// Scheme alone is not a boundary. `https://169.254.169.254/` is HTTPS and is
/// the cloud metadata service; `https://10.0.0.1/` is HTTPS and is inside the
/// deployment's own network. A tenant registering either would make this
/// gateway a request forwarder into infrastructure the tenant cannot otherwise
/// reach, with the gateway's own network position.
///
/// So the host is resolved and every address it resolves to is checked. A name
/// that resolves to one public address and one private one is refused on the
/// private one: allowing it because "at least one was fine" is the shape a DNS
/// rebinding attack needs.
///
/// Loopback is refused unless the deployment opts in. It is not harmless: a
/// tenant registering `http://127.0.0.1:8080` would make the gateway call its
/// own localhost, which is where admin and debug ports live. Development and
/// tests need it, so it is a configuration switch rather than a carve-out --
/// default deny, opted into where it is safe.
pub(crate) fn require_permitted_endpoint(
    endpoint: &str,
    loopback_permitted: bool,
) -> Result<(), McpFederationError> {
    resolve_permitted_endpoint(endpoint, loopback_permitted).map(drop)
}

/// Builds the connector from the exact address set that passed the outbound
/// policy check. OAuth discovery/token calls share this helper so credential
/// endpoints cannot re-resolve after validation either.
pub(crate) fn build_pinned_http_client_for_endpoint(
    endpoint: &str,
    request_timeout: Duration,
    loopback_permitted: bool,
) -> Result<reqwest::Client, McpFederationError> {
    let pinned = resolve_permitted_endpoint(endpoint, loopback_permitted)?;
    let mut builder = reqwest::Client::builder()
        .timeout(request_timeout)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    if let Some(pinned) = pinned {
        // TLS continues to authenticate the original hostname while the
        // connector receives only the addresses checked above.
        builder = builder.resolve_to_addrs(&pinned.hostname, &pinned.addresses);
    }
    builder
        .build()
        .map_err(|error| McpFederationError::Unreachable(error.to_string()))
}

/// Resolves and validates a hostname once, returning the exact addresses the
/// HTTP connector must use.
///
/// Returning `None` means the URL contains an IP literal, so reqwest has no DNS
/// decision to repeat. A hostname always returns a pin, including `localhost`
/// when a development deployment explicitly permits it.
fn resolve_permitted_endpoint(
    endpoint: &str,
    loopback_permitted: bool,
) -> Result<Option<PinnedDns>, McpFederationError> {
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|_| McpFederationError::EndpointNotPermitted(endpoint.to_owned()))?;
    let refuse = || McpFederationError::EndpointNotPermitted(endpoint.to_owned());
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(refuse());
    }
    let host = parsed.host_str().ok_or_else(refuse)?;
    let literal = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
    // An explicit literal or the conventional localhost name only. A different
    // name that happens to resolve to loopback is still a rebinding attempt.
    let declared_loopback = host.eq_ignore_ascii_case("localhost")
        || literal.is_some_and(|address| address.is_loopback());
    if declared_loopback {
        if !loopback_permitted {
            return Err(refuse());
        }
        if literal.is_some() {
            return Ok(None);
        }
        let addresses = resolve_addresses(host, parsed.port_or_known_default().unwrap_or(80))
            .map_err(|_| refuse())?;
        if addresses.is_empty() || addresses.iter().any(|address| !address.ip().is_loopback()) {
            return Err(refuse());
        }
        return Ok(Some(PinnedDns {
            hostname: host.to_owned(),
            addresses,
        }));
    }
    if parsed.scheme() != "https" {
        return Err(refuse());
    }
    if let Some(address) = literal {
        return if is_publicly_routable(address) {
            Ok(None)
        } else {
            Err(refuse())
        };
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addresses = resolve_addresses(host, port).map_err(|_| refuse())?;
    for address in &addresses {
        if !is_publicly_routable(address.ip()) {
            return Err(refuse());
        }
    }
    // A name that resolves to nothing is not "fine by default".
    if addresses.is_empty() {
        Err(refuse())
    } else {
        Ok(Some(PinnedDns {
            hostname: host.to_owned(),
            addresses,
        }))
    }
}

fn resolve_addresses(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    (host, port).to_socket_addrs().map(Iterator::collect)
}

/// Whether an address is one a tenant could have reached without us.
///
/// Written as a deny-by-default match over the ranges that are not, rather than
/// an allow-list of the ones that are: a range nobody thought about should end
/// up refused, and with an allow-list it would end up permitted.
fn is_publicly_routable(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 169.254.169.254 is link-local and already covered, but
                // carrier-grade NAT (100.64/10) and the benchmarking range are
                // not, and both sit inside real deployments.
                || matches!(v4.octets(), [100, b, ..] if (64..128).contains(&b))
                || matches!(v4.octets(), [198, 18..=19, ..])
                || matches!(v4.octets(), [192, 0, 0, _])
                || v4.octets()[0] == 0)
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local (fc00::/7) and link-local (fe80::/10).
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped: judge the address it actually reaches.
                || v6.to_ipv4_mapped().is_some_and(|mapped| {
                    !is_publicly_routable(std::net::IpAddr::V4(mapped))
                }))
        }
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn record_progress_frame(
    frame: &serde_json::Value,
    lifecycle: &McpCallLifecycle,
    last_progress: &mut Option<f64>,
) -> Result<(), McpFederationError> {
    let params = &frame["params"];
    if params["progressToken"] != lifecycle.progress_token {
        return Ok(());
    }
    let progress = params["progress"].as_f64().ok_or_else(|| {
        McpFederationError::Protocol("MCP progress must be a finite number".into())
    })?;
    if !progress.is_finite() || last_progress.is_some_and(|last| progress <= last) {
        return Err(McpFederationError::Protocol(
            "MCP progress must increase monotonically".into(),
        ));
    }
    let total = params["total"].as_f64();
    if total.is_some_and(|total| !total.is_finite()) {
        return Err(McpFederationError::Protocol(
            "MCP progress total must be finite".into(),
        ));
    }
    let message = params["message"].as_str().map(str::to_owned);
    if message
        .as_ref()
        .is_some_and(|message| message.len() > 2_048)
    {
        return Err(McpFederationError::Protocol(
            "MCP progress message exceeded 2048 bytes".into(),
        ));
    }
    *last_progress = Some(progress);
    let _ = lifecycle.progress.try_send(McpProgressNotification {
        progress,
        total,
        message,
    });
    Ok(())
}

fn catalog_digest(capabilities: &BTreeSet<McpServerCapability>, tools: &[McpTool]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agent-runtime-mcp-directory-v2\0");
    for capability in capabilities {
        hasher.update(capability.as_str().as_bytes());
        hasher.update([0]);
    }
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

    /// The policy lookup and the connector lookup must be the same lookup.
    ///
    /// `example.com` resolves publicly during the policy check, while this
    /// deliberately hostile HTTP client resolves it to a loopback listener at
    /// connect time. The first implementation accepted the public answer and
    /// then let reqwest perform the second, attacker-controlled lookup.
    #[tokio::test]
    async fn a_request_cannot_re_resolve_to_an_address_that_was_not_validated() {
        use rsa::rand_core::OsRng;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok(Ok((socket, _))) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
            {
                accepted_tx.send(()).ok();
                drop(socket);
            }
        });

        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(250))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .resolve("example.com", local_address)
            .build()
            .unwrap();
        let client = McpFederationClient {
            private_key: Some(RsaPrivateKey::new(&mut OsRng, 2048).unwrap()),
            key_id: Some("test-only".into()),
            http,
            request_timeout: Duration::from_millis(250),
            loopback_permitted: false,
            oauth_coordinator: None,
            modern_request_sequence: Arc::new(AtomicU64::new(1)),
        };
        let server = McpServerRef {
            server_id: Uuid::now_v7(),
            name: "search".into(),
            endpoint: format!("https://example.com:{}/rpc", local_address.port()),
            credential_envelope_json: String::new(),
            oauth_credential_id: None,
            protocol_revision: McpProtocolRevision::V2025_06_18,
            client_capabilities: BTreeSet::new(),
        };

        let _ = client.list_tools(Uuid::now_v7(), &server).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), accepted_rx)
                .await
                .is_err(),
            "the connector used a second DNS answer and reached loopback"
        );
    }

    #[test]
    fn a_plain_http_endpoint_off_loopback_is_refused() {
        assert!(require_permitted_endpoint("http://mcp.example.com/rpc", true).is_err());
        assert!(require_permitted_endpoint("http://127.0.0.1:8931/rpc", true).is_ok());
    }

    /// Loopback is a deployment decision, not a property of the URL. The gateway
    /// can reach its own admin ports; a tenant must not be able to make it.
    #[test]
    fn loopback_is_refused_unless_the_deployment_permits_it() {
        for endpoint in [
            "http://127.0.0.1:8931/rpc",
            "http://localhost:8080/rpc",
            "https://[::1]/rpc",
        ] {
            assert!(
                require_permitted_endpoint(endpoint, false).is_err(),
                "{endpoint} must be refused when loopback is not permitted"
            );
            assert!(
                require_permitted_endpoint(endpoint, true).is_ok(),
                "{endpoint} must be allowed when it is"
            );
        }
    }

    /// HTTPS is not a permission. Each of these is a valid HTTPS URL that the
    /// first version accepted, and each points somewhere a tenant could not
    /// otherwise reach.
    #[test]
    fn https_to_infrastructure_the_tenant_cannot_otherwise_reach_is_refused() {
        for hostile in [
            "https://169.254.169.254/latest/meta-data/",
            "https://[fd00::1]/rpc",
            "https://10.0.0.1/rpc",
            "https://192.168.1.1/rpc",
            "https://172.16.0.1/rpc",
            "https://100.64.0.1/rpc",
            "https://0.0.0.0/rpc",
            "https://[::ffff:10.0.0.1]/rpc",
        ] {
            assert!(
                require_permitted_endpoint(hostile, true).is_err(),
                "{hostile} must be refused"
            );
        }
    }

    #[test]
    fn a_public_address_is_still_permitted() {
        assert!(require_permitted_endpoint("https://93.184.216.34/rpc", false).is_ok());
    }

    #[test]
    fn credentials_in_the_endpoint_are_refused() {
        assert!(require_permitted_endpoint("https://user:pass@mcp.example.com/rpc", true).is_err());
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
        let capabilities = BTreeSet::from([McpServerCapability::Tools]);
        assert_ne!(
            catalog_digest(&capabilities, &one),
            catalog_digest(&capabilities, &two)
        );
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
        let capabilities = BTreeSet::from([McpServerCapability::Tools]);
        assert_eq!(
            catalog_digest(&capabilities, &one),
            catalog_digest(&capabilities, &two)
        );
    }

    #[test]
    fn the_digest_binds_the_advertised_server_surface() {
        let tools = vec![McpTool {
            qualified_name: "mcp:search/web".into(),
            description: "search".into(),
            input_schema_json: "{}".into(),
        }];
        let tools_only = BTreeSet::from([McpServerCapability::Tools]);
        let with_resources =
            BTreeSet::from([McpServerCapability::Tools, McpServerCapability::Resources]);

        assert_ne!(
            catalog_digest(&tools_only, &tools),
            catalog_digest(&with_resources, &tools),
            "a server must not add a callable surface inside a frozen Run"
        );
    }

    #[test]
    fn modern_discovery_accepts_a_resource_only_server_without_inventing_tools() {
        let capabilities = validate_modern_discovery(&serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"resources": {"listChanged": true}}
        }))
        .expect("resource-only discovery is valid");

        assert_eq!(
            capabilities,
            BTreeSet::from([McpServerCapability::Resources])
        );
        assert!(
            empty_catalog_for_capabilities(capabilities)
                .tools
                .is_empty()
        );
    }

    #[test]
    fn a_malformed_supported_capability_fails_closed() {
        let error = validate_modern_discovery(&serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"resources": true}
        }))
        .expect_err("a boolean capability is not a negotiated capability object");

        assert!(matches!(error, McpFederationError::Protocol(_)));
    }

    #[test]
    fn resource_pages_preserve_opaque_cursors_and_decode_typed_descriptors() {
        let page = resource_page_from_list_result(&serde_json::json!({
            "resources": [{
                "uri": "kb://tenant-a/runbook",
                "name": "runbook",
                "title": "Operations runbook",
                "description": "read-only evidence",
                "mimeType": "text/markdown",
                "size": 123
            }],
            "nextCursor": " opaque/+/= cursor "
        }))
        .expect("a bounded page is valid");

        assert_eq!(page.resources[0].uri, "kb://tenant-a/runbook");
        assert_eq!(page.resources[0].size, Some(123));
        assert_eq!(page.next_cursor.as_deref(), Some(" opaque/+/= cursor "));
    }

    #[test]
    fn resource_read_decodes_blob_bytes_and_refuses_ambiguous_content() {
        let result = resource_read_from_result(&serde_json::json!({
            "contents": [{
                "uri": "blob://artifact/1",
                "mimeType": "application/octet-stream",
                "blob": "AAEC"
            }]
        }))
        .expect("bounded base64 content is valid");
        assert!(matches!(
            &result.contents[0],
            McpResourceContent::Blob { bytes, .. } if bytes == &[0, 1, 2]
        ));

        let error = resource_read_from_result(&serde_json::json!({
            "contents": [{"uri": "x://y", "text": "x", "blob": "eA=="}]
        }))
        .expect_err("a content item cannot be both text and blob");
        assert!(matches!(error, McpFederationError::Protocol(_)));
    }

    #[test]
    fn resource_templates_preserve_server_uri_semantics_and_bounds() {
        let page = resource_template_page_from_list_result(&serde_json::json!({
            "resourceTemplates": [{
                "uriTemplate": "kb://tenant/{name}",
                "name": "knowledge",
                "title": "Knowledge item",
                "mimeType": "text/markdown"
            }],
            "nextCursor": "template-page-2"
        }))
        .expect("a bounded resource template page is valid");
        assert_eq!(
            page.resource_templates[0].uri_template,
            "kb://tenant/{name}"
        );
        assert_eq!(page.next_cursor.as_deref(), Some("template-page-2"));

        let too_many = vec![
            serde_json::json!({"uriTemplate": "kb://{name}", "name": "knowledge"});
            MAX_DIRECTORY_ENTRIES + 1
        ];
        assert!(
            resource_template_page_from_list_result(&serde_json::json!({
                "resourceTemplates": too_many
            }))
            .is_err()
        );
    }

    #[test]
    fn prompt_pages_and_results_are_bounded_protocol_neutral_values() {
        let page = prompt_page_from_list_result(&serde_json::json!({
            "prompts": [{
                "name": "summarize",
                "description": "Summarize one source",
                "arguments": [{"name": "tone", "required": false}]
            }],
            "nextCursor": "page-2"
        }))
        .expect("a bounded prompt page is valid");
        assert_eq!(page.prompts[0].arguments[0].name, "tone");
        assert_eq!(page.next_cursor.as_deref(), Some("page-2"));

        let result = prompt_result_from_get_result(&serde_json::json!({
            "description": "resolved",
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": "Summarize this"}
            }]
        }))
        .expect("a bounded prompt result is valid");
        assert_eq!(result.messages[0].role, "user");

        let unsupported = prompt_result_from_get_result(&serde_json::json!({
            "messages": [{"role": "system", "content": {"type": "text", "text": "x"}}]
        }))
        .expect_err("an MCP prompt cannot smuggle a system role");
        assert!(matches!(unsupported, McpFederationError::Protocol(_)));
    }

    #[test]
    fn pagination_and_directory_limits_fail_closed_without_partial_results() {
        assert!(validate_cursor(Some("")).is_err());
        assert!(validate_cursor(Some(&"x".repeat(MAX_CURSOR_BYTES + 1))).is_err());

        let resources = (0..=MAX_DIRECTORY_ENTRIES)
            .map(|index| serde_json::json!({"uri": format!("kb://{index}"), "name": "r"}))
            .collect::<Vec<_>>();
        assert!(
            resource_page_from_list_result(&serde_json::json!({
                "resources": resources
            }))
            .is_err()
        );
    }
}
