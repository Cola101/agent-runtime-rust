//! Generated wire types for the Runtime's own invocation surface.
//!
//! Kept in its own crate for the same reason `agent-model-gateway-protocol` is:
//! a Java SDK, a CLI or a sidecar links the contract without linking the
//! Runtime, and the Runtime cannot quietly change the wire shape by editing
//! its own internals.
pub mod v1 {
    tonic::include_proto!("agent.runtime.v1");
}
