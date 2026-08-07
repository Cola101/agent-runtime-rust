use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_runtime_worker::{ToolExecutionSupervisor, ToolExecutionUpdate};
use agent_tool_runtime::{
    ToolExecutionContext, ToolExecutionError, ToolExecutionResult, ToolExecutor,
};
use chrono::Utc;
use serde_json::json;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug)]
struct ImmediateExecutor;

impl ToolExecutor for ImmediateExecutor {
    fn implementation_digest(&self) -> &str {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        _context: ToolExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>
    {
        Box::pin(async move {
            Ok(ToolExecutionResult {
                content: json!({"url":request.call.arguments["url"],"status":200}),
                is_error: false,
                exit_code: 0,
            })
        })
    }
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "call_http_9".into(),
            name: "http".into(),
            arguments: json!({"url":"https://example.com"}),
        },
        effect: ToolEffect::Idempotent,
        sandbox: SandboxClass::RestrictedContainer,
        binding_digest: "d".repeat(64),
    }
}

fn context(attempt_id: Uuid) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id,
        workspace_root: PathBuf::from("/tmp"),
        timeout: Duration::from_secs(5),
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

#[tokio::test]
async fn one_bound_tool_call_is_launched_once_and_returns_its_identity() {
    let attempt_id = Uuid::now_v7();
    let mut supervisor = ToolExecutionSupervisor::new(8);

    assert!(supervisor.start(Arc::new(ImmediateExecutor), request(), context(attempt_id)));
    assert!(!supervisor.start(Arc::new(ImmediateExecutor), request(), context(attempt_id)));

    let update = supervisor.recv(Duration::from_secs(1)).await.unwrap();
    let ToolExecutionUpdate::Finished {
        attempt_id: actual_attempt,
        tool_call_id,
        binding_digest,
        result,
    } = update
    else {
        panic!("executor success must produce a finished update");
    };
    assert_eq!(actual_attempt, attempt_id);
    assert_eq!(tool_call_id, "call_http_9");
    assert_eq!(binding_digest, "d".repeat(64));
    assert_eq!(
        result.content,
        json!({"url":"https://example.com","status":200})
    );
}
