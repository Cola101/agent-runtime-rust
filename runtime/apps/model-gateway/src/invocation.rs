//! Transport-neutral decoding of a wire `ModelInvocation` into a `ModelRequest`.
//!
//! Both transports need this: the gRPC service decodes what a remote Worker
//! sent, and an embedded host decodes what its in-process Worker core prepared.
//! Keeping one decoder stops the two from drifting into sending the model
//! different transcripts for the same Run.

use agent_model_gateway_protocol::v1::{
    ModelInvocation, ModelRole, ReasoningPolicy as WireReasoningPolicy, content_part::Body,
};
use agent_protocol::{ContentPart, Message, ModelRequest, ReasoningPolicy, Role, ToolSpec};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelInvocationDecodeError {
    #[error("model role is unspecified")]
    UnspecifiedRole,
    #[error("tool result content is not valid JSON")]
    InvalidToolResultContent,
    #[error("tool call arguments are not valid JSON")]
    InvalidToolCallArguments,
    #[error("unsupported model content part")]
    UnsupportedContentPart,
    #[error("tool input schema is not valid JSON")]
    InvalidToolSchema,
    #[error("tool name must not be blank")]
    BlankToolName,
    #[error("structured output is not supported yet")]
    StructuredOutputUnsupported,
    #[error("reasoning policy is unspecified")]
    UnspecifiedReasoningPolicy,
    #[error("model invocation requires messages and a positive token limit")]
    EmptyInvocation,
}

pub fn decode_model_invocation(
    invocation: &ModelInvocation,
) -> Result<ModelRequest, ModelInvocationDecodeError> {
    let messages = invocation
        .messages
        .iter()
        .map(|message| {
            let role = match ModelRole::try_from(message.role) {
                Ok(ModelRole::System) => Role::System,
                Ok(ModelRole::User) => Role::User,
                Ok(ModelRole::Assistant) => Role::Assistant,
                Ok(ModelRole::Tool) => Role::Tool,
                _ => return Err(ModelInvocationDecodeError::UnspecifiedRole),
            };
            let content = message
                .content
                .iter()
                .map(|part| match &part.body {
                    Some(Body::Text(text)) => Ok(ContentPart::Text {
                        text: text.text.clone(),
                    }),
                    Some(Body::ToolResult(result)) => Ok(ContentPart::ToolResult {
                        tool_call_id: result.tool_call_id.clone(),
                        content: serde_json::from_slice(&result.content_json)
                            .map_err(|_| ModelInvocationDecodeError::InvalidToolResultContent)?,
                    }),
                    Some(Body::ToolCall(call)) => Ok(ContentPart::ToolCall {
                        tool_call_id: call.tool_call_id.clone(),
                        name: call.name.clone(),
                        arguments: serde_json::from_slice(&call.arguments_json)
                            .map_err(|_| ModelInvocationDecodeError::InvalidToolCallArguments)?,
                    }),
                    _ => Err(ModelInvocationDecodeError::UnsupportedContentPart),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Message { role, content })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tools = invocation
        .tools
        .iter()
        .map(|tool| {
            let input_schema = serde_json::from_slice(&tool.input_schema_json)
                .map_err(|_| ModelInvocationDecodeError::InvalidToolSchema)?;
            if tool.name.trim().is_empty() {
                return Err(ModelInvocationDecodeError::BlankToolName);
            }
            Ok(ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !invocation.output_schema_json.is_empty() {
        return Err(ModelInvocationDecodeError::StructuredOutputUnsupported);
    }
    let reasoning = match WireReasoningPolicy::try_from(invocation.reasoning) {
        Ok(WireReasoningPolicy::Minimal) => ReasoningPolicy::Minimal,
        Ok(WireReasoningPolicy::Balanced) => ReasoningPolicy::Balanced,
        Ok(WireReasoningPolicy::Thorough) => ReasoningPolicy::Thorough,
        _ => return Err(ModelInvocationDecodeError::UnspecifiedReasoningPolicy),
    };
    if messages.is_empty() || invocation.max_output_tokens == 0 {
        return Err(ModelInvocationDecodeError::EmptyInvocation);
    }
    Ok(ModelRequest {
        messages,
        tools,
        output_schema: None,
        reasoning,
        max_output_tokens: invocation.max_output_tokens,
    })
}
