//! Defines the RunInference wire messages used by Cursor's local agent loop.
//!
//! Field numbers and shapes mirror `aiserver.v1` in Cursor 3.31. Only the
//! fields the bridge reads or writes are declared; prost skips the rest.
#![allow(clippy::large_enum_variant)]

pub const MESSAGE_ROLE_UNSPECIFIED: i32 = 0;
pub const MESSAGE_ROLE_USER: i32 = 1;
pub const MESSAGE_ROLE_ASSISTANT: i32 = 2;
pub const MESSAGE_ROLE_TOOL: i32 = 3;
pub const MESSAGE_ROLE_SYSTEM: i32 = 4;

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceClientMessage {
    #[prost(
        oneof = "run_inference_client_message::Message",
        tags = "1,2,3,4,5,6,7,9,10,11"
    )]
    pub message: Option<run_inference_client_message::Message>,
}

pub mod run_inference_client_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(super::RunInferenceRunRequest),
        #[prost(message, tag = "2")]
        InvokeModel(super::RunInferenceInvokeModel),
        #[prost(message, tag = "3")]
        CancelInvocation(super::RunInferenceCancelInvocation),
        #[prost(message, tag = "4")]
        FinishRun(super::RunInferenceFinishRun),
        #[prost(message, tag = "5")]
        ClientHeartbeat(super::RunInferenceClientHeartbeat),
        #[prost(message, tag = "6")]
        AgentEvent(super::RunInferenceAgentEvent),
        #[prost(message, tag = "7")]
        RunRequestLog(super::RunInferenceRunRequestLog),
        #[prost(message, tag = "9")]
        ResolveSubagentModels(super::RunInferenceResolveSubagentModels),
        #[prost(message, tag = "10")]
        ClientFirstCallTimings(super::RunInferenceClientFirstCallTimings),
        #[prost(message, tag = "11")]
        DeltaBaseSeed(super::RunInferenceDeltaBaseSeed),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceRunRequest {
    #[prost(string, tag = "1")]
    pub conversation_id: String,
    #[prost(string, optional, tag = "2")]
    pub conversation_group_id: Option<String>,
    #[prost(message, optional, tag = "3")]
    pub requested_model: Option<InferenceRequestedModel>,
    #[prost(string, optional, tag = "5")]
    pub agent_mode: Option<String>,
    #[prost(bool, optional, tag = "9")]
    pub is_summarize_action: Option<bool>,
    #[prost(string, optional, tag = "12")]
    pub previous_request_id: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvokeModel {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub request: Option<InferenceStreamRequest>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceCancelInvocation {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceFinishRun {
    #[prost(bool, tag = "1")]
    pub aborted: bool,
    #[prost(bool, optional, tag = "2")]
    pub turn_completed: Option<bool>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceClientHeartbeat {}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceAgentEvent {
    #[prost(string, tag = "1")]
    pub method_name: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceRunRequestLog {
    #[prost(bytes = "vec", tag = "1")]
    pub agent_run_request: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceResolveSubagentModels {
    #[prost(string, tag = "1")]
    pub resolution_id: String,
    #[prost(string, repeated, tag = "2")]
    pub requested_models: Vec<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceClientFirstCallTimings {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceDeltaBaseSeed {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceServerMessage {
    #[prost(oneof = "run_inference_server_message::Message", tags = "1,2,3,4")]
    pub message: Option<run_inference_server_message::Message>,
}

pub mod run_inference_server_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Heartbeat(super::RunInferenceHeartbeat),
        #[prost(message, tag = "2")]
        RunReady(super::RunInferenceRunReady),
        #[prost(message, tag = "3")]
        InvocationResponse(super::RunInferenceInvocationResponse),
        #[prost(message, tag = "4")]
        InvocationEnd(super::RunInferenceInvocationEnd),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceHeartbeat {
    #[prost(bool, tag = "1")]
    pub accepts_speculative_invocations: bool,
    #[prost(bool, tag = "2")]
    pub accepts_compressed_speculative_invocations: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceRunReady {
    #[prost(message, optional, tag = "1")]
    pub resolved_model: Option<InferenceRequestedModel>,
    #[prost(bool, tag = "2")]
    pub supports_self_summary: bool,
    #[prost(string, optional, tag = "3")]
    pub routed_model_display_name: Option<String>,
    #[prost(message, optional, tag = "4")]
    pub prompt_model_metadata: Option<RunInferencePromptModelMetadata>,
}

/// Model-family facts Cursor's local loop uses to build its agent prompt.
/// `vendor` and `prompt_version` are mandatory; the client reports
/// "The server sent no prompt metadata" when either is missing.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferencePromptModelMetadata {
    #[prost(string, tag = "1")]
    pub vendor: String,
    #[prost(string, tag = "2")]
    pub prompt_version: String,
    #[prost(bool, tag = "3")]
    pub is_sonnet45: bool,
    #[prost(bool, tag = "4")]
    pub is_gemini3: bool,
    #[prost(bool, tag = "5")]
    pub is_gpt51: bool,
    #[prost(bool, tag = "6")]
    pub is_gpt52: bool,
    #[prost(bool, tag = "7")]
    pub is_gpt5: bool,
    #[prost(bool, tag = "8")]
    pub is_gpt55: bool,
    #[prost(bool, tag = "9")]
    pub is_gpt56: bool,
    #[prost(bool, tag = "10")]
    pub is_sonnet4: bool,
    #[prost(bool, tag = "11")]
    pub is_codex_family: bool,
    #[prost(bool, tag = "12")]
    pub is_gpt54: bool,
    #[prost(bool, tag = "13")]
    pub is_gpt52_codex: bool,
    #[prost(bool, tag = "14")]
    pub is_gpt53_codex: bool,
    #[prost(bool, tag = "15")]
    pub is_gpt53_codex_spark: bool,
    #[prost(bool, tag = "16")]
    pub is_claude4x: bool,
    #[prost(bool, tag = "17")]
    pub is_opus45: bool,
    #[prost(bool, tag = "18")]
    pub is_opus46: bool,
    #[prost(bool, tag = "19")]
    pub is_opus48: bool,
    #[prost(bool, tag = "20")]
    pub is_opus5: bool,
    #[prost(bool, tag = "21")]
    pub is_fable5: bool,
    #[prost(bool, tag = "22")]
    pub is_fruitcake: bool,
    #[prost(bool, tag = "23")]
    pub is_gpt5_family: bool,
    #[prost(bool, tag = "24")]
    pub is_composer1: bool,
    #[prost(bool, tag = "25")]
    pub is_composer15: bool,
    #[prost(bool, tag = "26")]
    pub is_composer2: bool,
    #[prost(bool, tag = "27")]
    pub is_composer_matterhorn: bool,
    #[prost(bool, tag = "28")]
    pub is_grok45_product_prompt: bool,
    #[prost(bool, tag = "29")]
    pub is_raw_training_slug: bool,
    #[prost(string, optional, tag = "30")]
    pub reasoning_effort: Option<String>,
    #[prost(bool, tag = "31")]
    pub use_dsv3_harness: bool,
    #[prost(int32, optional, tag = "32")]
    pub agent_token_limit: Option<i32>,
    #[prost(int32, optional, tag = "33")]
    pub estimated_cache_ttl_ms: Option<i32>,
    #[prost(bool, tag = "34")]
    pub is_grok46_product_prompt: bool,
    #[prost(string, tag = "35")]
    pub persona: String,
    #[prost(int32, optional, tag = "38")]
    pub progress_reminder_threshold: Option<i32>,
    #[prost(string, optional, tag = "39")]
    pub effort_level: Option<String>,
    #[prost(bool, tag = "40")]
    pub supports_assistant_message_prefill: bool,
    #[prost(bool, tag = "41")]
    pub enable_line_numbers: bool,
    #[prost(bool, tag = "42")]
    pub use_sparse_read_line_numbers: bool,
    #[prost(bool, tag = "43")]
    pub is_fable51: bool,
    #[prost(string, tag = "44")]
    pub self_identity_name: String,
    #[prost(message, repeated, tag = "46")]
    pub agent_mode_configs: Vec<RunInferenceAgentModeConfig>,
    #[prost(bool, optional, tag = "47")]
    pub use_new_plan_mode_prompts: Option<bool>,
    #[prost(bool, tag = "50")]
    pub is_opus55: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceAgentModeConfig {
    #[prost(string, tag = "1")]
    pub mode_id: String,
    #[prost(bool, tag = "2")]
    pub enable_ask_question_tool: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvocationResponse {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub response: Option<InferenceStreamResponse>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvocationEnd {
    #[prost(string, tag = "1")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "2")]
    pub error: Option<RunInferenceInvocationError>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceInvocationError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(message, repeated, tag = "3")]
    pub details: Vec<RunInferenceErrorDetail>,
    #[prost(int32, tag = "4")]
    pub refusal: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RunInferenceErrorDetail {
    #[prost(string, tag = "1")]
    pub r#type: String,
    #[prost(bytes = "vec", tag = "2")]
    pub value: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceRequestedModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(bool, tag = "2")]
    pub max_mode: bool,
    #[prost(message, repeated, tag = "3")]
    pub parameters: Vec<InferenceModelParameterValue>,
    #[prost(bool, tag = "4")]
    pub built_in_model: bool,
}

pub use crate::cursor::protocol::proto::agent::v1::requested_model::ModelParameterValue as InferenceModelParameterValue;

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceStreamRequest {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<InferenceCoreMessage>,
    #[prost(message, repeated, tag = "2")]
    pub tools: Vec<InferenceAgentTool>,
    #[prost(message, optional, tag = "4")]
    pub model_config: Option<InferenceModelConfig>,
    #[prost(string, tag = "5")]
    pub model_id: String,
    #[prost(string, tag = "6")]
    pub invocation_id: String,
    #[prost(message, optional, tag = "7")]
    pub requested_model: Option<InferenceRequestedModel>,
    #[prost(string, tag = "8")]
    pub conversation_id: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceCoreMessage {
    #[prost(int32, tag = "1")]
    pub role: i32,
    #[prost(oneof = "inference_core_message::Content", tags = "2,3,6")]
    pub content: Option<inference_core_message::Content>,
    #[prost(message, repeated, tag = "4")]
    pub tool_calls: Vec<InferenceToolCall>,
    #[prost(message, repeated, tag = "7")]
    pub reasoning_parts: Vec<InferenceReasoningPart>,
}

pub mod inference_core_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Content {
        #[prost(string, tag = "2")]
        Text(String),
        #[prost(message, tag = "3")]
        Parts(super::InferenceContentParts),
        #[prost(message, tag = "6")]
        ToolContent(super::InferenceToolResultContent),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceContentParts {
    #[prost(message, repeated, tag = "1")]
    pub parts: Vec<InferenceContentPart>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceContentPart {
    #[prost(oneof = "inference_content_part::Part", tags = "1,2,3")]
    pub part: Option<inference_content_part::Part>,
}

pub mod inference_content_part {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Part {
        #[prost(message, tag = "1")]
        Text(super::InferenceTextPart),
        #[prost(message, tag = "2")]
        Image(super::InferenceImagePart),
        #[prost(message, tag = "3")]
        File(super::InferenceFilePart),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceTextPart {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceImagePart {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, optional, tag = "2")]
    pub mime_type: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceFilePart {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, tag = "2")]
    pub media_type: String,
    #[prost(string, optional, tag = "3")]
    pub filename: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolResultContent {
    #[prost(message, repeated, tag = "1")]
    pub parts: Vec<InferenceToolResultPart>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolResultPart {
    #[prost(string, tag = "1")]
    pub tool_call_id: String,
    #[prost(string, tag = "2")]
    pub tool_name: String,
    #[prost(message, optional, tag = "3")]
    pub result: Option<::prost_types::Value>,
    #[prost(bool, tag = "4")]
    pub is_error: bool,
    #[prost(message, repeated, tag = "5")]
    pub experimental_content: Vec<InferenceContentPart>,
    #[prost(bool, optional, tag = "7")]
    pub cursor_tool_call_is_error: Option<bool>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolCall {
    #[prost(string, tag = "1")]
    pub tool_call_id: String,
    #[prost(string, tag = "2")]
    pub tool_name: String,
    #[prost(message, optional, tag = "3")]
    pub args: Option<::prost_types::Struct>,
    #[prost(string, optional, tag = "4")]
    pub raw_tool_call_args: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceReasoningPart {
    #[prost(bool, tag = "1")]
    pub is_redacted: bool,
    #[prost(string, tag = "2")]
    pub text: String,
    #[prost(string, optional, tag = "3")]
    pub signature: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub redacted_data: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceAgentTool {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(message, optional, tag = "3")]
    pub parameters: Option<::prost_types::Struct>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceModelConfig {
    #[prost(int32, optional, tag = "1")]
    pub max_tokens: Option<i32>,
    #[prost(float, optional, tag = "2")]
    pub temperature: Option<f32>,
    #[prost(float, optional, tag = "3")]
    pub top_p: Option<f32>,
    #[prost(string, repeated, tag = "4")]
    pub stop_sequences: Vec<String>,
    #[prost(int32, optional, tag = "5")]
    pub top_k: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceStreamResponse {
    #[prost(oneof = "inference_stream_response::Response", tags = "1,2,3,4,5,8,9")]
    pub response: Option<inference_stream_response::Response>,
}

pub mod inference_stream_response {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Response {
        #[prost(message, tag = "1")]
        TextPart(super::InferenceTextStreamPart),
        #[prost(message, tag = "2")]
        ToolCallPart(super::InferenceToolCallStreamPart),
        #[prost(message, tag = "3")]
        Usage(super::InferenceUsageInfo),
        #[prost(message, tag = "4")]
        ResponseInfo(super::InferenceResponseInfo),
        #[prost(message, tag = "5")]
        ExtendedUsage(super::InferenceExtendedUsageInfo),
        #[prost(message, tag = "8")]
        Error(super::InferenceStreamError),
        #[prost(message, tag = "9")]
        ThinkingPart(super::InferenceThinkingStreamPart),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceResponseInfo {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub model: String,
    #[prost(int64, tag = "3")]
    pub created_at: i64,
    #[prost(message, repeated, tag = "4")]
    pub messages: Vec<InferenceResponseMessage>,
    #[prost(string, optional, tag = "5")]
    pub error_message: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceResponseMessage {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(int32, tag = "2")]
    pub role: i32,
    #[prost(string, optional, tag = "3")]
    pub content: Option<String>,
    #[prost(message, repeated, tag = "4")]
    pub tool_calls: Vec<InferenceToolCall>,
    #[prost(message, repeated, tag = "6")]
    pub reasoning_parts: Vec<InferenceReasoningPart>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceTextStreamPart {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(bool, tag = "2")]
    pub is_final: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceThinkingStreamPart {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, optional, tag = "2")]
    pub signature: Option<String>,
    #[prost(bool, tag = "3")]
    pub is_final: bool,
    #[prost(string, optional, tag = "6")]
    pub redacted_data: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceToolCallStreamPart {
    #[prost(string, tag = "1")]
    pub tool_call_id: String,
    #[prost(string, tag = "2")]
    pub tool_name: String,
    #[prost(string, tag = "3")]
    pub args: String,
    #[prost(bool, tag = "4")]
    pub is_complete: bool,
    #[prost(int32, optional, tag = "5")]
    pub tool_index: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceUsageInfo {
    #[prost(int32, tag = "1")]
    pub prompt_tokens: i32,
    #[prost(int32, tag = "2")]
    pub completion_tokens: i32,
    #[prost(int32, optional, tag = "3")]
    pub total_tokens: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceExtendedUsageInfo {
    #[prost(int32, tag = "1")]
    pub input_tokens: i32,
    #[prost(int32, tag = "2")]
    pub output_tokens: i32,
    #[prost(int32, tag = "3")]
    pub cache_read_tokens: i32,
    #[prost(int32, tag = "4")]
    pub cache_write_tokens: i32,
    #[prost(int32, tag = "5")]
    pub max_tokens: i32,
    #[prost(int32, optional, tag = "6")]
    pub reasoning_tokens: Option<i32>,
    #[prost(int32, optional, tag = "7")]
    pub context_input_tokens: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InferenceStreamError {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(bool, tag = "3")]
    pub is_input_token_limit_error: bool,
}
