//! Stable, transport-neutral client port for embedding the Runtime.
//!
//! Desktop, CLI and Java adapters should depend on this minimal client port
//! instead of calling the much larger [`crate::embedded::EmbeddedRuntime`]
//! implementation API.  Authentication and profile construction remain the
//! adapter's responsibility; once a profile is selected, every transport gets
//! the same submit, control, cursor and watch semantics here.

use crate::admission::RuntimeAdmissionError;
use crate::embedded::{
    EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY, EmbeddedEventSubscription, EmbeddedRuntime,
    EmbeddedRuntimeError, RUNTIME_EVENT_CURSOR_MAX_EVENTS, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
    RuntimeControlCommand, RuntimeControlReceipt, RuntimeEventCursorErrorCode,
    RuntimeEventCursorPage, RuntimeEventCursorRequest, RuntimeEventCursorState,
    RuntimeEventStreamItem,
};
use agent_protocol::RuntimeInvocationContext;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

pub const RUNTIME_CLIENT_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_CLIENT_CONTRACT_VERSION: u32 = 1;
/// Equal to the `RunExecutionCommand` validation bound. Advertising a larger
/// edge limit would durably accept work the Kernel will later refuse.
pub const RUNTIME_CLIENT_MAX_INPUT_BYTES: usize = 32_000;
pub const RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES: usize = 64 * 1024;
pub const RUNTIME_CLIENT_MAX_REQUIRED_CAPABILITIES: usize = 32;
pub const RUNTIME_CLIENT_MAX_CAPABILITY_BYTES: usize = 128;

pub const RUNTIME_CAPABILITY_RUN_SUBMIT: &str = "run.submit.v1";
pub const RUNTIME_CAPABILITY_RUN_CONTROL: &str = "run.control.v1";
pub const RUNTIME_CAPABILITY_EVENTS_CURSOR: &str = "events.cursor.v1";
pub const RUNTIME_CAPABILITY_EVENTS_WATCH: &str = "events.watch.v1";
pub const RUNTIME_CAPABILITY_RECOVERY_STARTUP: &str = "recovery.startup.v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientHello {
    pub schema_version: u32,
    pub min_contract_version: u32,
    pub max_contract_version: u32,
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientDescriptor {
    pub schema_version: u32,
    pub contract_version: u32,
    pub runtime_version: String,
    pub capabilities: BTreeSet<String>,
    pub max_input_bytes: u64,
    pub max_action_json_bytes: u64,
    pub max_event_page_size: u32,
    pub max_event_stream_capacity: u32,
}

impl RuntimeClientDescriptor {
    #[must_use]
    pub fn current() -> Self {
        Self {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            runtime_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: BTreeSet::from([
                RUNTIME_CAPABILITY_RUN_SUBMIT.into(),
                RUNTIME_CAPABILITY_RUN_CONTROL.into(),
                RUNTIME_CAPABILITY_EVENTS_CURSOR.into(),
                RUNTIME_CAPABILITY_EVENTS_WATCH.into(),
                RUNTIME_CAPABILITY_RECOVERY_STARTUP.into(),
            ]),
            max_input_bytes: RUNTIME_CLIENT_MAX_INPUT_BYTES as u64,
            max_action_json_bytes: RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES as u64,
            max_event_page_size: RUNTIME_EVENT_CURSOR_MAX_EVENTS as u32,
            max_event_stream_capacity: EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY as u32,
        }
    }

    fn negotiate(hello: &RuntimeClientHello) -> Result<Self, RuntimeClientError> {
        if hello.schema_version != RUNTIME_CLIENT_SCHEMA_VERSION
            || hello.min_contract_version == 0
            || hello.max_contract_version < hello.min_contract_version
            || hello.required_capabilities.len() > RUNTIME_CLIENT_MAX_REQUIRED_CAPABILITIES
            || hello.required_capabilities.iter().any(|capability| {
                capability.is_empty()
                    || capability.len() > RUNTIME_CLIENT_MAX_CAPABILITY_BYTES
                    || !capability.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'-' | b'_')
                    })
            })
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime client initialization",
            ));
        }
        if !(hello.min_contract_version..=hello.max_contract_version)
            .contains(&RUNTIME_CLIENT_CONTRACT_VERSION)
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::UnsupportedContract,
                "Runtime client contract versions do not overlap",
            ));
        }
        let descriptor = Self::current();
        if !hello
            .required_capabilities
            .is_subset(&descriptor.capabilities)
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::UnsupportedContract,
                "Runtime does not provide every required client capability",
            ));
        }
        Ok(descriptor)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubmitRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub input: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubmitReceipt {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub owner_epoch: u64,
    /// The same actionable lifecycle boundary returned by event cursors. In
    /// particular, a pending approval is not exposed until the old execution
    /// owner has released the Run.
    pub state: RuntimeEventCursorState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientEventCursorRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub after_sequence: u64,
    pub limit: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeClientErrorCode {
    InvalidRequest,
    UnsupportedContract,
    Forbidden,
    Conflict,
    ResourceExhausted,
    NotFound,
    CursorAhead,
    DataLoss,
    Unavailable,
    Internal,
}

/// Sanitized adapter-facing error. Host paths, provider details and credential
/// material never cross this boundary; trusted operators can still use the
/// lower-level Embedded Runtime diagnostics in their own logs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[error("Runtime client {code:?}: {message}")]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientError {
    pub code: RuntimeClientErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientRecoveryFailure {
    pub invocation: RuntimeInvocationContext,
    pub error: RuntimeClientError,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientRecoveryReport {
    pub scanned_profiles: u64,
    pub recovered_runs: u64,
    pub failures: Vec<RuntimeClientRecoveryFailure>,
}

impl RuntimeClientError {
    fn new(code: RuntimeClientErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn from_embedded(error: EmbeddedRuntimeError) -> Self {
        match error {
            EmbeddedRuntimeError::UnregisteredInvocation => Self::new(
                RuntimeClientErrorCode::Forbidden,
                "this invocation is not registered",
            ),
            EmbeddedRuntimeError::ControlCommandRebound => Self::new(
                RuntimeClientErrorCode::Conflict,
                "this command id is already bound to a different command",
            ),
            EmbeddedRuntimeError::InvalidControlCommand(_) => Self::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime control command",
            ),
            EmbeddedRuntimeError::Admission(RuntimeAdmissionError::Closed) => Self::new(
                RuntimeClientErrorCode::Unavailable,
                "the Runtime stopped before granting admission",
            ),
            EmbeddedRuntimeError::Admission(_) => Self::new(
                RuntimeClientErrorCode::ResourceExhausted,
                "the Runtime is at its admission ceiling",
            ),
            EmbeddedRuntimeError::EventCursor(cursor) => match cursor.code {
                RuntimeEventCursorErrorCode::UnsupportedSchema => Self::new(
                    RuntimeClientErrorCode::UnsupportedContract,
                    "unsupported event cursor schema",
                ),
                RuntimeEventCursorErrorCode::InvalidRequest => Self::new(
                    RuntimeClientErrorCode::InvalidRequest,
                    "invalid event cursor request",
                ),
                RuntimeEventCursorErrorCode::NotFound => {
                    Self::new(RuntimeClientErrorCode::NotFound, "no such Run")
                }
                RuntimeEventCursorErrorCode::CursorAhead => Self::new(
                    RuntimeClientErrorCode::CursorAhead,
                    "cursor is ahead of the committed log",
                ),
                RuntimeEventCursorErrorCode::IdentityMismatch => Self::new(
                    RuntimeClientErrorCode::Forbidden,
                    "this Run belongs to another invocation",
                ),
                RuntimeEventCursorErrorCode::CorruptLog => {
                    Self::new(RuntimeClientErrorCode::DataLoss, "the event log is corrupt")
                }
                RuntimeEventCursorErrorCode::StorageUnavailable => Self::new(
                    RuntimeClientErrorCode::Unavailable,
                    "the event log is unavailable",
                ),
            },
            EmbeddedRuntimeError::Configuration(_) | EmbeddedRuntimeError::Runtime(_) => Self::new(
                RuntimeClientErrorCode::Internal,
                "the Runtime could not complete this request",
            ),
        }
    }
}

/// Bounded client stream that keeps the Embedded subscription implementation
/// and its host-local errors behind the stable client contract.
pub struct RuntimeClientEventSubscription {
    inner: EmbeddedEventSubscription,
}

impl RuntimeClientEventSubscription {
    pub async fn recv(&mut self) -> Option<Result<RuntimeEventStreamItem, RuntimeClientError>> {
        self.inner
            .recv()
            .await
            .map(|result| result.map_err(RuntimeClientError::from_embedded))
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.inner.capacity() as u32
    }
}

/// Negotiation entry point for a Tauri command layer, an Electron sidecar
/// adapter, a CLI or the gRPC service. Only the initialized result exposes
/// execution methods; neither type exposes Runtime configuration, paths,
/// credentials, Host handles or mutable profile state.
#[derive(Clone)]
pub struct RuntimeClient {
    runtime: Arc<EmbeddedRuntime>,
}

impl RuntimeClient {
    #[must_use]
    pub fn new(runtime: Arc<EmbeddedRuntime>) -> Self {
        Self { runtime }
    }

    pub fn initialize(
        &self,
        hello: &RuntimeClientHello,
    ) -> Result<InitializedRuntimeClient, RuntimeClientError> {
        let descriptor = RuntimeClientDescriptor::negotiate(hello)?;
        Ok(InitializedRuntimeClient {
            runtime: Arc::clone(&self.runtime),
            descriptor,
        })
    }
}

/// A negotiated Runtime client. Execution methods exist only on this type, so
/// an in-process UI cannot accidentally skip version/capability negotiation
/// and then create durable Run state.
#[derive(Clone)]
pub struct InitializedRuntimeClient {
    runtime: Arc<EmbeddedRuntime>,
    descriptor: RuntimeClientDescriptor,
}

impl InitializedRuntimeClient {
    #[must_use]
    pub fn descriptor(&self) -> &RuntimeClientDescriptor {
        &self.descriptor
    }

    pub async fn submit(
        &self,
        request: RuntimeSubmitRequest,
    ) -> Result<RuntimeSubmitReceipt, RuntimeClientError> {
        if request.schema_version != RUNTIME_CLIENT_SCHEMA_VERSION
            || request.run_id.is_nil()
            || request.input.trim().is_empty()
            || request.input.len() > RUNTIME_CLIENT_MAX_INPUT_BYTES
            || request.invocation.validate().is_err()
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime submit request",
            ));
        }
        let record = self
            .runtime
            .execute_detached(request.invocation, request.run_id, request.input)
            .await
            .map_err(RuntimeClientError::from_embedded)?;
        let page = self
            .runtime
            .event_cursor(RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: request.invocation,
                run_id: request.run_id,
                after_sequence: 0,
                limit: 1,
            })
            .map_err(RuntimeClientError::from_embedded)?;
        Ok(RuntimeSubmitReceipt {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: request.invocation,
            run_id: record.run_id,
            owner_epoch: record.owner_epoch,
            state: page.state,
        })
    }

    pub async fn control(
        &self,
        command: RuntimeControlCommand,
    ) -> Result<RuntimeControlReceipt, RuntimeClientError> {
        let action_bytes = serde_json::to_vec(&command.action).map_err(|_| {
            RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime control action",
            )
        })?;
        if action_bytes.len() > RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "Runtime control action exceeds its bound",
            ));
        }
        self.runtime
            .control_detached(command)
            .await
            .map_err(RuntimeClientError::from_embedded)
    }

    pub fn read_events(
        &self,
        request: RuntimeClientEventCursorRequest,
    ) -> Result<RuntimeEventCursorPage, RuntimeClientError> {
        self.runtime
            .event_cursor(RuntimeEventCursorRequest {
                schema_version: request.schema_version,
                invocation: request.invocation,
                run_id: request.run_id,
                after_sequence: request.after_sequence,
                limit: request.limit as usize,
            })
            .map_err(RuntimeClientError::from_embedded)
    }

    pub fn watch_events(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        after_sequence: u64,
        capacity: u32,
    ) -> Result<RuntimeClientEventSubscription, RuntimeClientError> {
        let inner = self
            .runtime
            .subscribe_events(invocation, run_id, after_sequence, capacity as usize)
            .map_err(RuntimeClientError::from_embedded)?;
        Ok(RuntimeClientEventSubscription { inner })
    }

    pub async fn recover_on_startup(&self) -> RuntimeClientRecoveryReport {
        let report = self.runtime.recover_all_unfinished_detached().await;
        RuntimeClientRecoveryReport {
            scanned_profiles: report.scanned_profiles as u64,
            recovered_runs: report.recovered_runs as u64,
            failures: report
                .failures
                .into_iter()
                .map(|failure| RuntimeClientRecoveryFailure {
                    invocation: failure.invocation,
                    error: RuntimeClientError::from_embedded(failure.error),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(required_capabilities: &[&str]) -> RuntimeClientHello {
        RuntimeClientHello {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            min_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            max_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            required_capabilities: required_capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
        }
    }

    #[test]
    fn initialization_negotiates_one_deterministic_bounded_contract() {
        let descriptor = RuntimeClientDescriptor::negotiate(&hello(&[
            RUNTIME_CAPABILITY_RUN_SUBMIT,
            RUNTIME_CAPABILITY_EVENTS_WATCH,
        ]))
        .expect("compatible client");

        assert_eq!(descriptor.contract_version, 1);
        assert_eq!(
            descriptor.capabilities.into_iter().collect::<Vec<_>>(),
            vec![
                "events.cursor.v1",
                "events.watch.v1",
                "recovery.startup.v1",
                "run.control.v1",
                "run.submit.v1",
            ]
        );
        assert_eq!(descriptor.max_input_bytes, 32_000);
        assert_eq!(
            descriptor.max_event_page_size,
            RUNTIME_EVENT_CURSOR_MAX_EVENTS as u32
        );
    }

    #[test]
    fn initialization_refuses_version_or_capability_guessing() {
        let mut incompatible = hello(&[]);
        incompatible.min_contract_version = 2;
        incompatible.max_contract_version = 3;
        assert_eq!(
            RuntimeClientDescriptor::negotiate(&incompatible)
                .expect_err("non-overlap")
                .code,
            RuntimeClientErrorCode::UnsupportedContract
        );

        let missing = hello(&["desktop.magic.v1"]);
        assert_eq!(
            RuntimeClientDescriptor::negotiate(&missing)
                .expect_err("missing capability")
                .code,
            RuntimeClientErrorCode::UnsupportedContract
        );

        let invalid = hello(&["UPPERCASE"]);
        assert_eq!(
            RuntimeClientDescriptor::negotiate(&invalid)
                .expect_err("invalid capability token")
                .code,
            RuntimeClientErrorCode::InvalidRequest
        );
    }

    #[test]
    fn stable_client_errors_do_not_expose_host_paths() {
        let error = RuntimeClientError::from_embedded(EmbeddedRuntimeError::Configuration(
            "failed to read /Users/private/.secrets/provider-key".into(),
        ));
        assert_eq!(error.code, RuntimeClientErrorCode::Internal);
        assert!(!error.message.contains("/Users/private"));
        assert!(!error.message.contains("provider-key"));
    }
}
