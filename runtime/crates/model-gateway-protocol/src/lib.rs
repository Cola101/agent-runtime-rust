pub mod v1 {
    tonic::include_proto!("agent.model.v1");
}

use sha2::{Digest, Sha256};

/// Stable digest of every authority-bearing field in an MCP server wire
/// snapshot. The concrete canonical encoding is part of the cross-language
/// workload-token contract.
#[must_use]
pub fn mcp_server_authorization_digest(server: &v1::McpServerRef) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agent-runtime-mcp-server-v1\0");
    update_field(&mut hasher, server.server_id.as_bytes());
    update_field(&mut hasher, server.name.as_bytes());
    update_field(&mut hasher, server.endpoint.as_bytes());
    update_field(&mut hasher, &server.credential_envelope_json);
    update_field(&mut hasher, server.protocol_revision.as_bytes());
    let mut capabilities = server.client_capabilities.iter().collect::<Vec<_>>();
    capabilities.sort_unstable();
    hasher.update((capabilities.len() as u64).to_be_bytes());
    for capability in capabilities {
        update_field(&mut hasher, capability.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn update_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}
