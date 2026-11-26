//! Bridges Cursor's local-loop inference messages to BYOK provider requests.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

use crate::{
    cursor::inference::{model::InferenceModel, wire::*},
    model::{
        ContentPart, ModelInvocation, ModelRequest, ModelSpec, ProjectedContent, ProjectedMessage,
        PromptSpec, Role, ToolCallContent, ToolDefinition, ToolResultContent, Usage,
    },
    Error, Result,
};

pub(super) struct SessionInfo<'a> {
    pub request_id: &'a str,
    pub conversation_id: &'a str,
    pub model: InferenceModel,
}

const REPLAY_CACHE_LIMIT: usize = 64;

/// Keeps provider replay state produced by one invocation until the client
/// sends that assistant turn back in a later invocation.
#[derive(Default)]
pub struct ReplayCache {
    entries: std::collections::VecDeque<(Vec<String>, crate::model::ProviderReplayState)>,
}

impl ReplayCache {
    pub fn insert(&mut self, call_ids: Vec<String>, state: crate::model::ProviderReplayState) {
        if call_ids.is_empty() {
            return;
        }
        self.entries.push_back((call_ids, state));
        while self.entries.len() > REPLAY_CACHE_LIMIT {
            self.entries.pop_front();
        }
    }

    fn find(
        &self,
        call_ids: &[String],
        provider_kind: &str,
    ) -> Option<&crate::model::ProviderReplayState> {
        self.entries
            .iter()
            .rev()
            .find(|(ids, state)| {
                state.provider_kind == provider_kind && ids.iter().any(|id| call_ids.contains(id))
            })
            .map(|(_, state)| state)
    }
}

const THINKING_CACHE_LIMIT: usize = 4096;

/// Maps Anthropic thinking signatures back to the text that produced them.
/// Cursor stores the signature as a separate reasoning part with empty text, so
/// replaying a conversation after a model switch would otherwise send an empty
/// thinking block and fail Anthropic's signature validation.
#[derive(Default)]
pub struct ThinkingTextCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl ThinkingTextCache {
    pub fn insert(&self, signature: impl Into<String>, text: impl Into<String>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries.len() >= THINKING_CACHE_LIMIT {
            entries.clear();
        }
        entries.insert(signature.into(), text.into());
    }

    fn get(&self, signature: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(signature)
            .cloned()
    }
}

pub fn run_ready(
    model: InferenceRequestedModel,
    display_name: Option<&str>,
    prompt_model_metadata: RunInferencePromptModelMetadata,
) -> RunInferenceServerMessage {
    RunInferenceServerMessage {
        message: Some(run_inference_server_message::Message::RunReady(
            RunInferenceRunReady {
                resolved_model: Some(model),
                supports_self_summary: true,
                routed_model_display_name: display_name.map(str::to_owned),
                prompt_model_metadata: Some(prompt_model_metadata),
            },
        )),
    }
}

/// Builds the model-family metadata Cursor's local loop requires. The client
/// rejects the turn with "The server sent no prompt metadata" when this is
/// absent, and with "unrecognized-model-family" when vendor or prompt_version
/// is not one of its known values.
pub fn prompt_metadata(
    model: &ModelSpec,
    config: Option<&crate::model::ModelConfig>,
) -> RunInferencePromptModelMetadata {
    let id = config
        .map_or(&model.model_id, |config| &config.model_id)
        .to_ascii_lowercase();
    let vendor = vendor_for(&id, config);
    let reasoning_effort = model.reasoning.effort.clone().filter(|effort| {
        matches!(
            effort.as_str(),
            "low" | "medium" | "high" | "extra-high" | "xhigh" | "max"
        )
    });
    let persona = if vendor == "xai" {
        ["grok-4.7", "grok-4.6", "grok-4.5"]
            .into_iter()
            .find(|persona| id.contains(persona))
            .unwrap_or_default()
            .to_owned()
    } else {
        String::new()
    };
    let agent_mode_configs = ["agent", "chat", "plan", "debug", "triage", "multitask"]
        .into_iter()
        .map(|mode_id| RunInferenceAgentModeConfig {
            mode_id: mode_id.into(),
            enable_ask_question_tool: true,
        })
        .collect();

    RunInferencePromptModelMetadata {
        vendor: vendor.into(),
        prompt_version: "latest".into(),
        is_sonnet45: matches_model(&id, &["sonnet-4-5", "sonnet-4.5"]),
        is_gemini3: vendor == "google" && id.contains('3'),
        is_gpt51: matches_model(&id, &["gpt-5-1", "gpt-5.1"]),
        is_gpt52: matches_model(&id, &["gpt-5-2", "gpt-5.2"]),
        is_gpt5: matches_model(&id, &["gpt-5-sol", "gpt-5"]),
        is_gpt55: matches_model(&id, &["gpt-5-5", "gpt-5.5"]),
        is_gpt56: matches_model(&id, &["gpt-5-6", "gpt-5.6"]),
        is_sonnet4: matches_model(&id, &["sonnet-4", "sonnet-4.0"]),
        is_codex_family: id.contains("codex"),
        is_gpt54: matches_model(&id, &["gpt-5-4", "gpt-5.4"]),
        is_gpt52_codex: id.contains("gpt-5.2-codex") || id.contains("gpt-5-2-codex"),
        is_gpt53_codex: id.contains("gpt-5.3-codex") || id.contains("gpt-5-3-codex"),
        is_gpt53_codex_spark: id.contains("codex-spark"),
        is_claude4x: [
            "claude-4",
            "claude-5",
            "claude-4.",
            "claude-5.",
            "opus-4",
            "opus-5",
            "sonnet-4",
            "sonnet-5",
        ]
        .iter()
        .any(|marker| id.contains(marker)),
        is_opus45: matches_model(&id, &["opus-4-5", "opus-4.5"]),
        is_opus46: matches_model(&id, &["opus-4-6", "opus-4.6"]),
        is_opus48: matches_model(&id, &["opus-4-8", "opus-4.8"]),
        is_opus5: matches_model(&id, &["opus-5-", "opus-5."]),
        is_fable5: id.contains("fable-5") && !id.contains("fable-5-1"),
        is_fruitcake: id.contains("fruitcake"),
        is_gpt5_family: vendor == "openai" && (id.contains("gpt-5") || id.contains("gpt-6")),
        is_grok45_product_prompt: id.contains("grok-4.5"),
        is_grok46_product_prompt: id.contains("grok-4.6"),
        reasoning_effort,
        agent_token_limit: model.context_window_tokens.map(wire_tokens),
        persona,
        self_identity_name: "Cursor".into(),
        agent_mode_configs,
        is_fable51: id.contains("fable-5-1"),
        is_opus55: matches_model(&id, &["opus-5-5", "opus-5.5"]),
        ..Default::default()
    }
}

fn vendor_for(id: &str, model: Option<&crate::model::ModelConfig>) -> &'static str {
    if model.is_some_and(|model| model.model_type == crate::model::ModelType::Anthropic) {
        return "anthropic";
    }
    if id.contains("claude") || id.contains("sonnet") || id.contains("opus") || id.contains("haiku")
    {
        "anthropic"
    } else if id.contains("grok") {
        "xai"
    } else if id.contains("gemini") {
        "google"
    } else if id.contains("gpt") || model.is_some() {
        "openai"
    } else {
        "unknown"
    }
}

fn matches_model(id: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| id.contains(marker))
}

pub fn heartbeat() -> RunInferenceServerMessage {
    RunInferenceServerMessage {
        message: Some(run_inference_server_message::Message::Heartbeat(
            RunInferenceHeartbeat::default(),
        )),
    }
}

pub fn invocation_response(
    invocation_id: &str,
    response: inference_stream_response::Response,
) -> RunInferenceServerMessage {
    RunInferenceServerMessage {
        message: Some(run_inference_server_message::Message::InvocationResponse(
            RunInferenceInvocationResponse {
                invocation_id: invocation_id.into(),
                response: Some(InferenceStreamResponse {
                    response: Some(response),
                }),
            },
        )),
    }
}

pub fn text_part(text: impl Into<String>, is_final: bool) -> inference_stream_response::Response {
    inference_stream_response::Response::TextPart(InferenceTextStreamPart {
        text: text.into(),
        is_final,
    })
}

pub fn thinking_part(
    text: impl Into<String>,
    is_final: bool,
) -> inference_stream_response::Response {
    inference_stream_response::Response::ThinkingPart(InferenceThinkingStreamPart {
        text: text.into(),
        is_final,
        ..Default::default()
    })
}

/// Carries Anthropic thinking signatures and redacted blocks back to Cursor so
/// the next invocation can replay the assistant turn verbatim.
pub fn replay_parts(
    state: &crate::model::ProviderReplayState,
) -> Vec<inference_stream_response::Response> {
    if state.provider_kind != "anthropic" {
        return Vec::new();
    }
    let Some(blocks) = state.value.get("blocks").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            let kind = block.get("type").and_then(Value::as_str)?;
            match kind {
                "thinking" => {
                    let signature = block.get("signature").and_then(Value::as_str)?;
                    Some(inference_stream_response::Response::ThinkingPart(
                        InferenceThinkingStreamPart {
                            text: String::new(),
                            signature: Some(signature.into()),
                            is_final: false,
                            ..Default::default()
                        },
                    ))
                }
                "redacted_thinking" => {
                    let data = block.get("data").and_then(Value::as_str)?;
                    Some(inference_stream_response::Response::ThinkingPart(
                        InferenceThinkingStreamPart {
                            text: String::new(),
                            redacted_data: Some(data.into()),
                            is_final: false,
                            ..Default::default()
                        },
                    ))
                }
                _ => None,
            }
        })
        .collect()
}

pub fn tool_call_part(
    call_id: &str,
    name: &str,
    arguments: &str,
    index: usize,
) -> inference_stream_response::Response {
    inference_stream_response::Response::ToolCallPart(InferenceToolCallStreamPart {
        tool_call_id: call_id.into(),
        tool_name: name.into(),
        args: arguments.into(),
        is_complete: true,
        tool_index: Some(index.min(i32::MAX as usize) as i32),
    })
}

pub fn tool_call_start(
    call_id: &str,
    name: &str,
    index: usize,
) -> inference_stream_response::Response {
    inference_stream_response::Response::ToolCallPart(InferenceToolCallStreamPart {
        tool_call_id: call_id.into(),
        tool_name: name.into(),
        args: String::new(),
        is_complete: false,
        tool_index: Some(index.min(i32::MAX as usize) as i32),
    })
}

pub fn tool_call_delta(
    call_id: &str,
    arguments: &str,
    index: usize,
) -> inference_stream_response::Response {
    inference_stream_response::Response::ToolCallPart(InferenceToolCallStreamPart {
        tool_call_id: call_id.into(),
        tool_name: String::new(),
        args: arguments.into(),
        is_complete: false,
        tool_index: Some(index.min(i32::MAX as usize) as i32),
    })
}

pub struct InvocationOutput {
    pub text: String,
    pub tool_calls: Vec<InferenceToolCall>,
    pub reasoning_parts: Vec<InferenceReasoningPart>,
}

pub fn response_info(
    invocation_id: &str,
    model: &str,
    output: &InvocationOutput,
) -> Option<inference_stream_response::Response> {
    if output.text.is_empty() && output.tool_calls.is_empty() && output.reasoning_parts.is_empty() {
        return None;
    }
    Some(inference_stream_response::Response::ResponseInfo(
        InferenceResponseInfo {
            id: invocation_id.into(),
            model: model.into(),
            created_at: chrono::Utc::now().timestamp(),
            messages: vec![InferenceResponseMessage {
                id: invocation_id.into(),
                role: MESSAGE_ROLE_ASSISTANT,
                content: (!output.text.is_empty()).then(|| output.text.clone()),
                tool_calls: output.tool_calls.clone(),
                reasoning_parts: output.reasoning_parts.clone(),
            }],
            error_message: None,
        },
    ))
}

pub fn reasoning_parts_from_state(
    state: &crate::model::ProviderReplayState,
) -> Vec<InferenceReasoningPart> {
    if state.provider_kind != "anthropic" {
        return Vec::new();
    }
    let Some(blocks) = state.value.get("blocks").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let signature = block.get("signature").and_then(Value::as_str)?;
                Some(InferenceReasoningPart {
                    text: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    signature: Some(signature.into()),
                    ..Default::default()
                })
            }
            Some("redacted_thinking") => Some(InferenceReasoningPart {
                is_redacted: true,
                redacted_data: block.get("data").and_then(Value::as_str).map(str::to_owned),
                ..Default::default()
            }),
            _ => None,
        })
        .collect()
}

pub fn tool_call_output(call_id: &str, name: &str, arguments: &str) -> InferenceToolCall {
    let parsed = serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object);
    InferenceToolCall {
        tool_call_id: call_id.into(),
        tool_name: name.into(),
        args: parsed.as_ref().map(json_to_struct),
        raw_tool_call_args: Some(arguments.to_owned()),
    }
}

fn json_to_struct(value: &Value) -> ::prost_types::Struct {
    ::prost_types::Struct {
        fields: value
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), json_to_prost(value)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn json_to_prost(value: &Value) -> ::prost_types::Value {
    use ::prost_types::value::Kind;
    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(value) => Kind::BoolValue(*value),
        Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap_or_default()),
        Value::String(value) => Kind::StringValue(value.clone()),
        Value::Array(values) => Kind::ListValue(::prost_types::ListValue {
            values: values.iter().map(json_to_prost).collect(),
        }),
        Value::Object(_) => Kind::StructValue(json_to_struct(value)),
    };
    ::prost_types::Value { kind: Some(kind) }
}

fn wire_tokens(tokens: u64) -> i32 {
    tokens.min(i32::MAX as u64) as i32
}

pub fn usage_parts(
    usage: &Usage,
    context_window: Option<u64>,
) -> Vec<inference_stream_response::Response> {
    let number = |value: Option<u64>| wire_tokens(value.unwrap_or_default());
    let mut parts = vec![inference_stream_response::Response::Usage(
        InferenceUsageInfo {
            prompt_tokens: number(usage.input_tokens),
            completion_tokens: number(usage.output_tokens),
            total_tokens: usage.total_tokens.map(wire_tokens),
        },
    )];
    parts.push(inference_stream_response::Response::ExtendedUsage(
        InferenceExtendedUsageInfo {
            input_tokens: number(usage.input_tokens),
            output_tokens: number(usage.output_tokens),
            cache_read_tokens: number(usage.cache_read_tokens),
            cache_write_tokens: number(usage.cache_write_tokens),
            max_tokens: number(context_window),
            reasoning_tokens: usage.reasoning_tokens.map(wire_tokens),
            context_input_tokens: usage.context_input_tokens.map(wire_tokens),
        },
    ));
    parts
}

pub fn invocation_end(
    invocation_id: &str,
    error: Option<(i32, String)>,
) -> RunInferenceServerMessage {
    RunInferenceServerMessage {
        message: Some(run_inference_server_message::Message::InvocationEnd(
            RunInferenceInvocationEnd {
                invocation_id: invocation_id.into(),
                error: error.map(|(code, message)| RunInferenceInvocationError {
                    code,
                    message,
                    ..Default::default()
                }),
            },
        )),
    }
}

pub fn input_token_limit(message: String) -> inference_stream_response::Response {
    inference_stream_response::Response::Error(InferenceStreamError {
        message,
        is_input_token_limit_error: true,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn invocation_from_request(
    session: SessionInfo<'_>,
    invocation_id: &str,
    request: &InferenceStreamRequest,
    provider_call_index: u64,
    replay: &ReplayCache,
    thinking: &ThinkingTextCache,
) -> Result<ModelInvocation> {
    let mut instructions = Vec::new();
    let mut seen_instructions = std::collections::HashSet::new();
    let mut history = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            MESSAGE_ROLE_SYSTEM => {
                if let Some(text) = message_text(message) {
                    if !text.is_empty() && seen_instructions.insert(text.clone()) {
                        instructions.push(text);
                    }
                }
            }
            MESSAGE_ROLE_ASSISTANT => history.push(assistant_message(
                message,
                index,
                session.model.is_anthropic,
                replay,
                thinking,
            )?),
            MESSAGE_ROLE_TOOL => {
                history.extend(tool_messages(message, index));
            }
            _ => history.push(user_message(message, index)),
        }
    }

    let tools = request
        .tools
        .iter()
        .map(tool_definition)
        .collect::<Vec<_>>();

    Ok(ModelInvocation {
        call_id: if invocation_id.is_empty() {
            format!("inference:{}:{provider_call_index}", session.request_id)
        } else {
            invocation_id.to_owned()
        },
        run_id: session.request_id.into(),
        conversation_id: if request.conversation_id.is_empty() {
            session.conversation_id.into()
        } else {
            request.conversation_id.clone()
        },
        provider_call_index,
        request: ModelRequest {
            prompt: PromptSpec {
                instructions: instructions.join("\n\n"),
                tools,
            },
            model: session.model.spec,
            history,
        },
    })
}

fn message_text(message: &InferenceCoreMessage) -> Option<String> {
    match message.content.as_ref()? {
        inference_core_message::Content::Text(text) => Some(text.clone()),
        inference_core_message::Content::Parts(parts) => Some(
            parts
                .parts
                .iter()
                .filter_map(part_text)
                .collect::<Vec<_>>()
                .join(""),
        ),
        inference_core_message::Content::ToolContent(_) => None,
    }
}

fn part_text(part: &InferenceContentPart) -> Option<String> {
    match part.part.as_ref()? {
        inference_content_part::Part::Text(text) => Some(text.text.clone()),
        inference_content_part::Part::File(file) => Some(match &file.filename {
            Some(name) => format!("[File: {name}]"),
            None => "[File]".into(),
        }),
        inference_content_part::Part::Image(_) => None,
    }
}

fn user_message(message: &InferenceCoreMessage, index: usize) -> ProjectedMessage {
    let parts = match message.content.as_ref() {
        Some(inference_core_message::Content::Text(text)) => {
            vec![ContentPart::Text { text: text.clone() }]
        }
        Some(inference_core_message::Content::Parts(parts)) => content_parts(&parts.parts),
        _ => Vec::new(),
    };
    ProjectedMessage {
        message_id: format!("inference:{index}"),
        role: Role::User,
        content: ProjectedContent::Parts(parts),
    }
}

fn assistant_message(
    message: &InferenceCoreMessage,
    index: usize,
    is_anthropic: bool,
    replay: &ReplayCache,
    thinking_cache: &ThinkingTextCache,
) -> Result<ProjectedMessage> {
    let text = match message.content.as_ref() {
        Some(inference_core_message::Content::Text(text)) => text.clone(),
        Some(inference_core_message::Content::Parts(parts)) => parts
            .parts
            .iter()
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    let thinking = message
        .reasoning_parts
        .iter()
        .filter(|part| !part.is_redacted)
        .map(|part| part.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    let calls = message
        .tool_calls
        .iter()
        .enumerate()
        .map(|(call_index, call)| {
            if call.tool_call_id.is_empty() || call.tool_name.is_empty() {
                return Err(Error::Protocol(
                    "RunInference assistant tool call is missing its id or name".into(),
                ));
            }
            Ok(ToolCallContent {
                index: call_index,
                call_id: call.tool_call_id.clone(),
                name: call.tool_name.clone(),
                arguments: tool_arguments(call),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let replay_state = if is_anthropic {
        anthropic_replay_state(&message.reasoning_parts, true, thinking_cache)
    } else {
        let call_ids = calls
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>();
        replay.find(&call_ids, "openai_responses").cloned()
    };
    Ok(ProjectedMessage {
        message_id: format!("inference:{index}"),
        role: Role::Assistant,
        content: ProjectedContent::Assistant {
            text,
            thinking,
            replay_state,
            calls,
        },
    })
}

fn anthropic_replay_state(
    parts: &[InferenceReasoningPart],
    is_anthropic: bool,
    thinking: &ThinkingTextCache,
) -> Option<crate::model::ProviderReplayState> {
    if !is_anthropic {
        return None;
    }
    let blocks = parts
        .iter()
        .filter_map(|part| {
            if part.is_redacted {
                let data = part.redacted_data.clone()?;
                Some(serde_json::json!({"type": "redacted_thinking", "data": data}))
            } else {
                // Only replay signatures this process produced with an
                // Anthropic model. Cursor also stores signed reasoning from
                // other providers (notably OpenAI Responses) in the same field;
                // sending those to Anthropic fails signature validation.
                let signature = part.signature.clone()?;
                let text = thinking.get(&signature)?;
                Some(serde_json::json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": signature,
                }))
            }
        })
        .collect::<Vec<_>>();
    (!blocks.is_empty()).then(|| crate::model::ProviderReplayState {
        provider_kind: "anthropic".into(),
        value: serde_json::json!({ "blocks": blocks }),
    })
}

fn tool_messages(message: &InferenceCoreMessage, index: usize) -> Vec<ProjectedMessage> {
    let Some(inference_core_message::Content::ToolContent(content)) = message.content.as_ref()
    else {
        return Vec::new();
    };
    content
        .parts
        .iter()
        .enumerate()
        .map(|(part_index, part)| {
            let (content, provider_parts) = tool_result_content(part);
            ProjectedMessage {
                message_id: format!("inference:{index}:{part_index}"),
                role: Role::Tool,
                content: ProjectedContent::ToolResult(ToolResultContent {
                    call_id: part.tool_call_id.clone(),
                    name: part.tool_name.clone(),
                    content,
                    is_error: part.is_error || part.cursor_tool_call_is_error.unwrap_or(false),
                    image: None,
                    provider_parts,
                }),
            }
        })
        .collect()
}

fn tool_result_content(part: &InferenceToolResultPart) -> (String, Vec<ContentPart>) {
    let mut content = part.result.as_ref().map(json_text).unwrap_or_default();
    let mut images = Vec::new();
    for content_part in &part.experimental_content {
        match content_part.part.as_ref() {
            Some(inference_content_part::Part::Image(image)) => {
                if let Some(part) = image_part(image) {
                    images.push(part);
                }
            }
            Some(inference_content_part::Part::Text(text)) if !text.text.is_empty() => {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&text.text);
            }
            _ => {}
        }
    }
    (content, images)
}

fn tool_arguments(call: &InferenceToolCall) -> Value {
    if let Some(args) = call.args.as_ref() {
        return json_struct(args);
    }
    if let Some(raw) = call.raw_tool_call_args.as_deref() {
        if let Ok(value) = serde_json::from_str::<Value>(raw) {
            return value;
        }
    }
    Value::Object(Default::default())
}

fn tool_definition(tool: &InferenceAgentTool) -> ToolDefinition {
    let parameters = tool
        .parameters
        .as_ref()
        .map(json_struct)
        .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
    ToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: normalize_tool_schema(parameters),
    }
}

/// Cursor wraps tool JSON schemas as `{"jsonSchema": <schema>, validate: ...}`;
/// the validator function is dropped by JSON serialization, leaving only
/// `jsonSchema`. Provider APIs reject that wrapper because its top level is not
/// an object schema, so unwrap it and guarantee a non-nullable object schema.
fn normalize_tool_schema(mut value: Value) -> Value {
    for _ in 0..4 {
        let Some(inner) = value
            .as_object()
            .and_then(|object| object.get("jsonSchema"))
            .filter(|inner| inner.is_object())
            .cloned()
        else {
            break;
        };
        value = inner;
    }
    let Some(object) = value.as_object_mut() else {
        return serde_json::json!({"type": "object", "properties": {}});
    };
    let object_type = object.get("type").cloned();
    match object_type {
        Some(Value::Array(types)) if types.iter().any(|value| value == "object") => {
            object.insert("type".into(), Value::String("object".into()));
        }
        Some(Value::String(value)) if value == "object" => {}
        Some(Value::String(value)) if value == "null" => {
            object.insert("type".into(), Value::String("object".into()));
            object
                .entry("properties")
                .or_insert_with(|| Value::Object(Default::default()));
        }
        Some(_) => {}
        None => {
            if object.contains_key("properties")
                || object.contains_key("required")
                || object.contains_key("additionalProperties")
            {
                object.insert("type".into(), Value::String("object".into()));
            } else if object.is_empty() {
                object.insert("type".into(), Value::String("object".into()));
                object.insert("properties".into(), Value::Object(Default::default()));
            }
        }
    }
    object.remove("nullable");
    value
}

fn content_parts(parts: &[InferenceContentPart]) -> Vec<ContentPart> {
    let mut output = Vec::new();
    for part in parts {
        match part.part.as_ref() {
            Some(inference_content_part::Part::Text(text)) => {
                if !text.text.is_empty() {
                    output.push(ContentPart::Text {
                        text: text.text.clone(),
                    });
                }
            }
            Some(inference_content_part::Part::Image(image)) => {
                if let Some(part) = image_part(image) {
                    output.push(part);
                }
            }
            Some(inference_content_part::Part::File(file)) => {
                output.push(ContentPart::Text {
                    text: match &file.filename {
                        Some(name) => format!("[File: {name}]"),
                        None => "[File]".into(),
                    },
                });
            }
            None => {}
        }
    }
    output
}

fn image_part(image: &InferenceImagePart) -> Option<ContentPart> {
    let (mime_type, payload) = match image.data.strip_prefix("data:") {
        Some(rest) => {
            let (meta, payload) = rest.split_once(',')?;
            let mime_type = meta
                .split(';')
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| image.mime_type.clone())?;
            (mime_type, payload)
        }
        None => (image.mime_type.clone()?, image.data.as_str()),
    };
    let data = STANDARD.decode(payload.trim()).ok()?;
    Some(ContentPart::Image { mime_type, data })
}

pub fn json_text(value: &::prost_types::Value) -> String {
    match &value.kind {
        Some(::prost_types::value::Kind::StringValue(text)) => text.clone(),
        _ => serde_json::to_string(&json_value(value)).unwrap_or_default(),
    }
}

pub fn json_struct(value: &::prost_types::Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), json_value(value)))
            .collect(),
    )
}

fn json_value(value: &::prost_types::Value) -> Value {
    use ::prost_types::value::Kind;
    match &value.kind {
        Some(Kind::NullValue(_)) | None => Value::Null,
        Some(Kind::NumberValue(number)) => {
            // protobuf Struct stores every number as a double. JSON Schema
            // constraints such as minItems/maxItems must serialize as integers,
            // so restore whole numbers before writing the provider request.
            if number.is_finite()
                && number.fract() == 0.0
                && *number >= i64::MIN as f64
                && *number <= i64::MAX as f64
            {
                Value::Number(serde_json::Number::from(*number as i64))
            } else {
                serde_json::Number::from_f64(*number)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
        }
        Some(Kind::StringValue(text)) => Value::String(text.clone()),
        Some(Kind::BoolValue(value)) => Value::Bool(*value),
        Some(Kind::StructValue(value)) => json_struct(value),
        Some(Kind::ListValue(values)) => {
            Value::Array(values.values.iter().map(json_value).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::prost_types::{value::Kind, Struct, Value as ProstValue};

    fn text_message(role: i32, text: &str) -> InferenceCoreMessage {
        InferenceCoreMessage {
            role,
            content: Some(inference_core_message::Content::Text(text.into())),
            ..Default::default()
        }
    }

    fn session() -> SessionInfo<'static> {
        SessionInfo {
            request_id: "request",
            conversation_id: "conversation",
            model: InferenceModel::new("byok-model", None),
        }
    }

    fn string_value(text: &str) -> ProstValue {
        ProstValue {
            kind: Some(Kind::StringValue(text.into())),
        }
    }

    #[test]
    fn request_maps_system_assistant_and_tool_messages() {
        let system = text_message(MESSAGE_ROLE_SYSTEM, "You are Cursor.");
        let duplicate_system = text_message(MESSAGE_ROLE_SYSTEM, "You are Cursor.");
        let user = text_message(MESSAGE_ROLE_USER, "hello");
        let assistant = InferenceCoreMessage {
            role: MESSAGE_ROLE_ASSISTANT,
            content: Some(inference_core_message::Content::Text("calling".into())),
            tool_calls: vec![InferenceToolCall {
                tool_call_id: "call-1".into(),
                tool_name: "Glob".into(),
                args: Some(Struct {
                    fields: [("glob_pattern".into(), string_value("**/*"))].into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let tool = InferenceCoreMessage {
            role: MESSAGE_ROLE_TOOL,
            content: Some(inference_core_message::Content::ToolContent(
                InferenceToolResultContent {
                    parts: vec![InferenceToolResultPart {
                        tool_call_id: "call-1".into(),
                        tool_name: "Glob".into(),
                        result: Some(string_value("src/main.rs")),
                        ..Default::default()
                    }],
                },
            )),
            ..Default::default()
        };

        let request = InferenceStreamRequest {
            messages: vec![system, duplicate_system, user, assistant, tool],
            tools: vec![InferenceAgentTool {
                name: "Glob".into(),
                description: "find files".into(),
                parameters: Some(Struct::default()),
            }],
            model_id: "byok-model".into(),
            conversation_id: "conversation".into(),
            ..Default::default()
        };

        let invocation = invocation_from_request(
            session(),
            "invocation-1",
            &request,
            0,
            &ReplayCache::default(),
            &ThinkingTextCache::default(),
        )
        .unwrap();
        assert_eq!(invocation.request.prompt.instructions, "You are Cursor.");
        assert_eq!(invocation.request.prompt.tools.len(), 1);
        assert_eq!(invocation.request.prompt.tools[0].name, "Glob");
        assert_eq!(invocation.request.history.len(), 3);
        let ProjectedContent::Assistant { text, calls, .. } =
            &invocation.request.history[1].content
        else {
            panic!("expected assistant projection");
        };
        assert_eq!(text, "calling");
        assert_eq!(calls[0].call_id, "call-1");
        assert_eq!(calls[0].arguments["glob_pattern"], "**/*");
        let ProjectedContent::ToolResult(result) = &invocation.request.history[2].content else {
            panic!("expected tool result projection");
        };
        assert_eq!(result.call_id, "call-1");
        assert_eq!(result.content, "src/main.rs");
    }

    #[test]
    fn image_data_url_becomes_image_part() {
        let request = InferenceStreamRequest {
            messages: vec![InferenceCoreMessage {
                role: MESSAGE_ROLE_USER,
                content: Some(inference_core_message::Content::Parts(
                    InferenceContentParts {
                        parts: vec![InferenceContentPart {
                            part: Some(inference_content_part::Part::Image(InferenceImagePart {
                                data: "data:image/png;base64,aGVsbG8=".into(),
                                mime_type: Some("image/png".into()),
                            })),
                        }],
                    },
                )),
                ..Default::default()
            }],
            ..Default::default()
        };

        let invocation = invocation_from_request(
            session(),
            "invocation-1",
            &request,
            0,
            &ReplayCache::default(),
            &ThinkingTextCache::default(),
        )
        .unwrap();
        let ProjectedContent::Parts(parts) = &invocation.request.history[0].content else {
            panic!("expected parts projection");
        };
        assert_eq!(
            parts,
            &[ContentPart::Image {
                mime_type: "image/png".into(),
                data: b"hello".to_vec()
            }]
        );
    }

    #[test]
    fn anthropic_reasoning_signatures_round_trip() {
        let parts = [InferenceReasoningPart {
            is_redacted: false,
            text: "thinking".into(),
            signature: Some("signature".into()),
            redacted_data: None,
        }];
        let cache = ThinkingTextCache::default();
        cache.insert("signature", "thinking");
        let state = anthropic_replay_state(&parts, true, &cache).unwrap();
        assert_eq!(state.provider_kind, "anthropic");

        let replay = replay_parts(&state);
        let inference_stream_response::Response::ThinkingPart(part) = &replay[0] else {
            panic!("expected thinking part");
        };
        assert_eq!(part.signature.as_deref(), Some("signature"));
        assert!(part.text.is_empty());
        assert!(replay_parts(&state).len() == 1);
    }

    #[test]
    fn thinking_replay_requires_a_signature_this_process_produced() {
        let parts = [InferenceReasoningPart {
            is_redacted: false,
            // Cursor also stores signed reasoning from other providers here;
            // non-empty text must not make an unknown signature replayable.
            text: "foreign thinking".into(),
            signature: Some("foreign-signature".into()),
            redacted_data: None,
        }];
        assert!(anthropic_replay_state(&parts, true, &ThinkingTextCache::default()).is_none());
    }

    #[test]
    fn thinking_replay_uses_cached_text_for_cursor_empty_parts() {
        let parts = [InferenceReasoningPart {
            is_redacted: false,
            text: String::new(),
            signature: Some("signature".into()),
            redacted_data: None,
        }];
        let cache = ThinkingTextCache::default();
        cache.insert("signature", "real thinking");
        let state = anthropic_replay_state(&parts, true, &cache).unwrap();
        assert_eq!(state.value["blocks"][0]["thinking"], "real thinking");
        assert_eq!(state.value["blocks"][0]["signature"], "signature");
    }

    #[test]
    fn non_anthropic_models_do_not_carry_replay_state() {
        let parts = [InferenceReasoningPart {
            is_redacted: false,
            text: "thinking".into(),
            signature: Some("signature".into()),
            redacted_data: None,
        }];
        assert!(anthropic_replay_state(&parts, false, &ThinkingTextCache::default()).is_none());
    }

    #[test]
    fn replay_state_is_attached_to_the_matching_assistant_turn() {
        let request = InferenceStreamRequest {
            messages: vec![InferenceCoreMessage {
                role: MESSAGE_ROLE_ASSISTANT,
                content: Some(inference_core_message::Content::Text("calling".into())),
                tool_calls: vec![InferenceToolCall {
                    tool_call_id: "call-1".into(),
                    tool_name: "Glob".into(),
                    args: Some(Struct::default()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            model_id: "byok-model".into(),
            ..Default::default()
        };
        let mut replay = ReplayCache::default();
        replay.insert(
            vec!["call-1".into()],
            crate::model::ProviderReplayState {
                provider_kind: "openai_responses".into(),
                value: serde_json::json!({
                    "items": [{
                        "type": "reasoning",
                        "id": "rs_1",
                        "encrypted_content": "secret"
                    }]
                }),
            },
        );

        let invocation = invocation_from_request(
            session(),
            "invocation-1",
            &request,
            0,
            &replay,
            &ThinkingTextCache::default(),
        )
        .unwrap();
        let ProjectedContent::Assistant { replay_state, .. } =
            &invocation.request.history[0].content
        else {
            panic!("expected assistant projection");
        };
        assert_eq!(
            replay_state
                .as_ref()
                .map(|state| state.provider_kind.as_str()),
            Some("openai_responses")
        );

        let mut other = ReplayCache::default();
        other.insert(
            vec!["call-other".into()],
            crate::model::ProviderReplayState {
                provider_kind: "openai_responses".into(),
                value: serde_json::json!({"items": []}),
            },
        );
        let invocation = invocation_from_request(
            session(),
            "invocation-2",
            &request,
            0,
            &other,
            &ThinkingTextCache::default(),
        )
        .unwrap();
        let ProjectedContent::Assistant { replay_state, .. } =
            &invocation.request.history[0].content
        else {
            panic!("expected assistant projection");
        };
        assert!(replay_state.is_none());
    }

    #[test]
    fn prompt_metadata_identifies_known_model_families() {
        let grok = prompt_metadata(&ModelSpec::new("grok-4.7"), None);
        assert_eq!(grok.vendor, "xai");
        assert_eq!(grok.persona, "grok-4.7");
        assert_eq!(grok.prompt_version, "latest");

        let claude = prompt_metadata(&ModelSpec::new("claude-opus-5-5"), None);
        assert_eq!(claude.vendor, "anthropic");
        assert!(claude.is_opus55);
        assert!(claude.is_claude4x);

        let gpt = prompt_metadata(&ModelSpec::new("gpt-6.1-sol"), None);
        assert_eq!(gpt.vendor, "openai");
        assert!(gpt.is_gpt5_family);
        assert!(gpt
            .agent_mode_configs
            .iter()
            .any(|mode| mode.mode_id == "agent"));
        assert!(gpt
            .agent_mode_configs
            .iter()
            .any(|mode| mode.mode_id == "plan"));
    }

    #[test]
    fn tool_schema_unwraps_cursor_json_schema_wrapper() {
        let normalized = normalize_tool_schema(serde_json::json!({
            "jsonSchema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }
        }));

        assert_eq!(normalized["type"], "object");
        assert!(normalized.get("jsonSchema").is_none());
        assert_eq!(normalized["properties"]["command"]["type"], "string");
    }

    #[test]
    fn tool_schema_normalizes_nullable_object_union() {
        let normalized = normalize_tool_schema(
            serde_json::json!({"type": ["object", "null"], "properties": {}}),
        );
        assert_eq!(normalized["type"], "object");
    }

    #[test]
    fn tool_schema_defaults_empty_and_null_only_schemas() {
        assert_eq!(
            normalize_tool_schema(serde_json::json!({}))["type"],
            "object"
        );
        assert_eq!(
            normalize_tool_schema(serde_json::json!({"type": "null"}))["type"],
            "object"
        );
    }

    #[test]
    fn whole_numbers_keep_integer_json_type() {
        let value = serde_json::json!({"minItems": 2, "minimum": 1.5});
        let round_trip = json_struct(&json_to_struct(&value));

        assert_eq!(round_trip["minItems"].to_string(), "2");
        assert_eq!(round_trip["minimum"].to_string(), "1.5");
    }

    #[test]
    fn usage_maps_to_both_usage_shapes() {
        let usage = Usage {
            input_tokens: Some(10),
            context_input_tokens: Some(10),
            output_tokens: Some(2),
            total_tokens: Some(12),
            cache_read_tokens: Some(4),
            cache_write_tokens: Some(3),
            reasoning_tokens: Some(1),
        };
        let parts = usage_parts(&usage, Some(272_000));
        assert_eq!(parts.len(), 2);
        let inference_stream_response::Response::Usage(basic) = &parts[0] else {
            panic!("expected basic usage");
        };
        assert_eq!(basic.prompt_tokens, 10);
        assert_eq!(basic.completion_tokens, 2);
        let inference_stream_response::Response::ExtendedUsage(extended) = &parts[1] else {
            panic!("expected extended usage");
        };
        assert_eq!(extended.cache_read_tokens, 4);
        assert_eq!(extended.reasoning_tokens, Some(1));
    }
}
