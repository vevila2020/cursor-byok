//! Accepts ordered Cursor Bidi append requests and routes them by request_id.
use bytes::Buf;
use prost::{encoding, Message};

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{
            events,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        transport::{TransportParent, TransportRegistry},
    },
    Error, Result,
};

pub struct DecodedAppend {
    pub request_id: String,
    pub seqno: i64,
    pub message: agent::AgentClientMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteInfo {
    pub request_id: String,
    pub model_id: Option<String>,
    pub conversation_id: Option<String>,
    pub is_run_request: bool,
}

struct NormalizedRunRequest {
    data_binary: Vec<u8>,
    removed_checkpoint_fields: Vec<u32>,
    removed_mcp_tool_fields: Vec<u32>,
    removed_skill_descriptor_fields: Vec<u32>,
    removed_run_request_fields: Vec<u32>,
    checkpoint_truncated_tail: bool,
    mcp_tools_truncated_tail: bool,
    skill_options_truncated_tail: bool,
    run_request_truncated_tail: bool,
}

#[derive(Clone, PartialEq, Message)]
struct AgentClientRouteHeader {
    #[prost(bytes = "vec", optional, tag = "1")]
    run_request: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct AgentRunRouteHeader {
    #[prost(bytes = "vec", optional, tag = "3")]
    model_details: Option<Vec<u8>>,
    #[prost(string, optional, tag = "5")]
    conversation_id: Option<String>,
    #[prost(bytes = "vec", optional, tag = "9")]
    requested_model: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelIdHeader {
    #[prost(string, tag = "1")]
    model_id: String,
}

/// Extracts fields needed for routing without decoding the conversation checkpoint.
pub fn route_info(request: &ai::BidiAppendRequest) -> Result<RouteInfo> {
    let request_id = request_id(request)?.to_owned();
    if request.data_binary.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend contains no AgentClientMessage".into(),
        ));
    }
    let client_header = AgentClientRouteHeader::decode(request.data_binary.as_slice())?;
    let Some(run_request) = client_header.run_request else {
        return Ok(RouteInfo {
            request_id,
            model_id: None,
            conversation_id: None,
            is_run_request: false,
        });
    };
    let run_header = AgentRunRouteHeader::decode(run_request.as_slice())?;
    let requested_model_id = run_header
        .requested_model
        .as_deref()
        .map(ModelIdHeader::decode)
        .transpose()?
        .map(|model| model.model_id)
        .filter(|model_id| !model_id.is_empty());
    let model_id = match requested_model_id {
        Some(model_id) => Some(model_id),
        None => run_header
            .model_details
            .as_deref()
            .map(ModelIdHeader::decode)
            .transpose()?
            .map(|model| model.model_id)
            .filter(|model_id| !model_id.is_empty()),
    };

    Ok(RouteInfo {
        request_id,
        model_id,
        conversation_id: run_header.conversation_id,
        is_run_request: true,
    })
}

impl DecodedAppend {
    pub fn model_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty())
            .or_else(|| {
                request
                    .model_details
                    .as_ref()
                    .map(|model| model.model_id.as_str())
                    .filter(|model| !model.is_empty())
            })
    }

    pub fn conversation_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request.conversation_id.as_deref()
    }

    pub fn is_background_task_completion(&self) -> bool {
        let Some(agent::agent_client_message::Message::RunRequest(request)) =
            self.message.message.as_ref()
        else {
            return false;
        };
        matches!(
            request
                .action
                .as_ref()
                .and_then(|action| action.action.as_ref()),
            Some(agent::conversation_action::Action::BackgroundTaskCompletionAction(_))
        )
    }

    pub fn trace_metadata(&self) -> serde_json::Value {
        let Some(message) = self.message.message.as_ref() else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": "empty",
            });
        };
        let agent::agent_client_message::Message::RunRequest(request) = message else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": client_message_type(message),
            });
        };
        let (action_type, history_messages, history_images) = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .map(|action| match action {
                agent::conversation_action::Action::UserMessageAction(action) => {
                    let history = action.conversation_history.as_ref();
                    (
                        "user_message",
                        history.map_or(0, |history| history.messages.len()),
                        history.map_or(0, history_image_count),
                    )
                }
                agent::conversation_action::Action::BackgroundTaskCompletionAction(_) => {
                    ("background_task_completion", 0, 0)
                }
                agent::conversation_action::Action::ExecutePlanAction(_) => ("execute_plan", 0, 0),
                agent::conversation_action::Action::SummarizeAction(_) => ("summarize", 0, 0),
                _ => ("other", 0, 0),
            })
            .unwrap_or(("none", 0, 0));
        let state = request.conversation_state.as_ref();
        serde_json::json!({
            "append_seqno": self.seqno,
            "message_type": "run_request",
            "conversation_id": request.conversation_id,
            "model_id": self.model_id(),
            "action_type": action_type,
            "conversation_history_messages": history_messages,
            "conversation_history_images": history_images,
            "root_message_count": state.map_or(0, |state| state.root_prompt_messages_json.len()),
            "turn_count": state.map_or(0, |state| state.turns.len()),
            "prefetched_blob_count": request.pre_fetched_blobs.len(),
        })
    }
}

fn client_message_type(message: &agent::agent_client_message::Message) -> &'static str {
    use agent::agent_client_message::Message;
    match message {
        Message::RunRequest(_) => "run_request",
        Message::ExecClientMessage(_) => "exec_client_message",
        Message::ExecClientControlMessage(_) => "exec_client_control_message",
        Message::KvClientMessage(_) => "kv_client_message",
        Message::ConversationAction(_) => "conversation_action",
        Message::InteractionResponse(_) => "interaction_response",
        Message::ClientHeartbeat(_) => "client_heartbeat",
        Message::PrewarmRequest(_) => "prewarm_request",
    }
}

fn history_image_count(history: &agent::ConversationHistory) -> usize {
    use agent::{
        conversation_history_message::Message,
        conversation_history_tool_result_content::Content as ToolContent,
        conversation_history_user_content::Content as UserContent,
    };
    history
        .messages
        .iter()
        .map(|message| match message.message.as_ref() {
            Some(Message::User(user)) => user
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(UserContent::Image(_))))
                .count(),
            Some(Message::Tool(tool)) => tool
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(ToolContent::Image(_))))
                .count(),
            _ => 0,
        })
        .sum()
}

pub fn decode(request: &ai::BidiAppendRequest) -> Result<DecodedAppend> {
    let request_id = request_id(request)?;
    if request.data_binary.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend contains no AgentClientMessage".into(),
        ));
    }
    let message = match agent::AgentClientMessage::decode(request.data_binary.as_slice()) {
        Ok(message) => message,
        Err(decode_error) => {
            let normalized = match normalize_run_request(&request.data_binary) {
                Ok(Some(normalized)) => normalized,
                Ok(None) => {
                    tracing::warn!(
                        request_id,
                        decode_error = %decode_error,
                        "Cursor BidiAppend decoding failed outside a recoverable run request field"
                    );
                    return Err(decode_error.into());
                }
                Err(normalize_error) => {
                    tracing::warn!(
                        request_id,
                        decode_error = %decode_error,
                        run_request_normalization_error = %normalize_error,
                        "failed to inspect Cursor run request after BidiAppend decoding failed"
                    );
                    return Err(normalize_error);
                }
            };
            tracing::warn!(
                request_id,
                removed_checkpoint_fields = ?normalized.removed_checkpoint_fields,
                removed_mcp_tool_fields = ?normalized.removed_mcp_tool_fields,
                removed_skill_descriptor_fields = ?normalized.removed_skill_descriptor_fields,
                removed_run_request_fields = ?normalized.removed_run_request_fields,
                checkpoint_truncated_tail = normalized.checkpoint_truncated_tail,
                mcp_tools_truncated_tail = normalized.mcp_tools_truncated_tail,
                skill_options_truncated_tail = normalized.skill_options_truncated_tail,
                run_request_truncated_tail = normalized.run_request_truncated_tail,
                "discarded incompatible Cursor run request fields for local BYOK decoding"
            );
            match agent::AgentClientMessage::decode(normalized.data_binary.as_slice()) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(
                        request_id,
                        original_decode_error = %decode_error,
                        normalized_decode_error = %error,
                        "Cursor BidiAppend remains invalid after run request normalization"
                    );
                    return Err(error.into());
                }
            }
        }
    };
    Ok(DecodedAppend {
        request_id: request_id.into(),
        seqno: request.append_seqno,
        message,
    })
}

fn request_id(request: &ai::BidiAppendRequest) -> Result<&str> {
    request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("BidiAppend request_id is required".into()))
}

fn normalize_run_request(payload: &[u8]) -> Result<Option<NormalizedRunRequest>> {
    let Some(run_request) = delimited_field(payload, 1)? else {
        return Ok(None);
    };
    let mut normalized_run_request = run_request.to_vec();
    let mut removed_checkpoint_fields = Vec::new();
    let mut removed_mcp_tool_fields = Vec::new();
    let mut removed_skill_descriptor_fields = Vec::new();
    let mut removed_run_request_fields = Vec::new();
    let mut checkpoint_truncated_tail = false;
    let mut mcp_tools_truncated_tail = false;
    let mut skill_options_truncated_tail = false;
    let mut run_request_truncated_tail = false;

    if let Some(conversation_state) = delimited_field(&normalized_run_request, 1)? {
        let (state, removed_fields, truncated_tail) =
            filter_conversation_state_fields(conversation_state);
        if state != conversation_state {
            normalized_run_request = replace_delimited_field(&normalized_run_request, 1, &state)?
                .ok_or_else(|| {
                Error::Protocol("cannot replace Cursor conversation checkpoint".into())
            })?;
            removed_checkpoint_fields = removed_fields;
            checkpoint_truncated_tail = truncated_tail;
        }
    }

    if let Some(mcp_tools) = delimited_field(&normalized_run_request, 4)? {
        let (tools, removed_fields, truncated_tail) = filter_mcp_tool_fields(mcp_tools);
        if tools != mcp_tools {
            normalized_run_request =
                replace_delimited_field(&normalized_run_request, 4, &tools)?
                    .ok_or_else(|| Error::Protocol("cannot replace Cursor MCP tools".into()))?;
            removed_mcp_tool_fields = removed_fields;
            mcp_tools_truncated_tail = truncated_tail;
        }
    }

    if let Some(skill_options) = delimited_field(&normalized_run_request, 7)? {
        let (skills, removed_fields, truncated_tail) = filter_skill_descriptors(skill_options);
        if skills != skill_options {
            normalized_run_request = replace_delimited_field(&normalized_run_request, 7, &skills)?
                .ok_or_else(|| Error::Protocol("cannot replace Cursor skill options".into()))?;
            removed_skill_descriptor_fields = removed_fields;
            skill_options_truncated_tail = truncated_tail;
        }
    }

    let (filtered_run_request, removed_fields, truncated_tail) =
        filter_run_request_fields(&normalized_run_request);
    if filtered_run_request != normalized_run_request {
        normalized_run_request = filtered_run_request;
        removed_run_request_fields = removed_fields;
        run_request_truncated_tail = truncated_tail;
    }

    if normalized_run_request == run_request {
        return Ok(None);
    }
    let payload = replace_delimited_field(payload, 1, &normalized_run_request)?
        .ok_or_else(|| Error::Protocol("cannot replace Cursor run request".into()))?;
    Ok(Some(NormalizedRunRequest {
        data_binary: payload,
        removed_checkpoint_fields,
        removed_mcp_tool_fields,
        removed_skill_descriptor_fields,
        removed_run_request_fields,
        checkpoint_truncated_tail,
        mcp_tools_truncated_tail,
        skill_options_truncated_tail,
        run_request_truncated_tail,
    }))
}

fn delimited_field(payload: &[u8], target_tag: u32) -> Result<Option<&[u8]>> {
    let mut remaining = payload;
    while remaining.has_remaining() {
        let (tag, wire_type) = encoding::decode_key(&mut remaining)?;
        if tag == target_tag && wire_type == encoding::WireType::LengthDelimited {
            let length = encoding::decode_varint(&mut remaining)?;
            let length = usize::try_from(length)
                .map_err(|_| Error::Protocol("protobuf field length is too large".into()))?;
            if length > remaining.remaining() {
                return Err(Error::Protocol("truncated protobuf field".into()));
            }
            let start = payload.len() - remaining.remaining();
            return Ok(Some(&payload[start..start + length]));
        }
        encoding::skip_field(
            wire_type,
            tag,
            &mut remaining,
            encoding::DecodeContext::default(),
        )?;
    }
    Ok(None)
}

fn filter_conversation_state_fields(payload: &[u8]) -> (Vec<u8>, Vec<u32>, bool) {
    let mut remaining = payload;
    let mut output = Vec::with_capacity(payload.len());
    let mut removed_fields = Vec::new();
    let mut truncated_tail = false;
    while remaining.has_remaining() {
        let start = payload.len() - remaining.remaining();
        let Ok((tag, wire_type)) = encoding::decode_key(&mut remaining) else {
            truncated_tail = true;
            break;
        };
        if encoding::skip_field(
            wire_type,
            tag,
            &mut remaining,
            encoding::DecodeContext::default(),
        )
        .is_err()
        {
            removed_fields.push(tag);
            truncated_tail = true;
            break;
        }
        let end = payload.len() - remaining.remaining();
        let field = &payload[start..end];
        if agent::ConversationStateStructure::decode(field).is_ok() {
            output.extend_from_slice(field);
        } else {
            removed_fields.push(tag);
        }
    }
    (output, removed_fields, truncated_tail)
}

fn filter_mcp_tool_fields(payload: &[u8]) -> (Vec<u8>, Vec<u32>, bool) {
    let mut remaining = payload;
    let mut output = Vec::with_capacity(payload.len());
    let mut removed_fields = Vec::new();
    let mut truncated_tail = false;
    while remaining.has_remaining() {
        let start = payload.len() - remaining.remaining();
        let Ok((tag, wire_type)) = encoding::decode_key(&mut remaining) else {
            truncated_tail = true;
            break;
        };
        if encoding::skip_field(
            wire_type,
            tag,
            &mut remaining,
            encoding::DecodeContext::default(),
        )
        .is_err()
        {
            removed_fields.push(tag);
            truncated_tail = true;
            break;
        }
        let end = payload.len() - remaining.remaining();
        let field = &payload[start..end];
        if agent::McpTools::decode(field).is_ok() {
            output.extend_from_slice(field);
        } else {
            removed_fields.push(tag);
        }
    }
    (output, removed_fields, truncated_tail)
}

fn filter_skill_descriptors(payload: &[u8]) -> (Vec<u8>, Vec<u32>, bool) {
    let mut remaining = payload;
    let mut output = Vec::with_capacity(payload.len());
    let mut removed_fields = Vec::new();
    let mut truncated_tail = false;
    while remaining.has_remaining() {
        let start = payload.len() - remaining.remaining();
        let Ok((tag, wire_type)) = encoding::decode_key(&mut remaining) else {
            truncated_tail = true;
            break;
        };
        if encoding::skip_field(
            wire_type,
            tag,
            &mut remaining,
            encoding::DecodeContext::default(),
        )
        .is_err()
        {
            removed_fields.push(tag);
            truncated_tail = true;
            break;
        }
        let end = payload.len() - remaining.remaining();
        let field = &payload[start..end];
        if agent::SkillOptions::decode(field).is_ok() {
            output.extend_from_slice(field);
        } else {
            removed_fields.push(tag);
        }
    }
    (output, removed_fields, truncated_tail)
}

fn filter_run_request_fields(payload: &[u8]) -> (Vec<u8>, Vec<u32>, bool) {
    let mut remaining = payload;
    let mut output = Vec::with_capacity(payload.len());
    let mut removed_fields = Vec::new();
    let mut truncated_tail = false;
    while remaining.has_remaining() {
        let start = payload.len() - remaining.remaining();
        let Ok((tag, wire_type)) = encoding::decode_key(&mut remaining) else {
            truncated_tail = true;
            break;
        };
        if encoding::skip_field(
            wire_type,
            tag,
            &mut remaining,
            encoding::DecodeContext::default(),
        )
        .is_err()
        {
            removed_fields.push(tag);
            truncated_tail = true;
            break;
        }
        let end = payload.len() - remaining.remaining();
        let field = &payload[start..end];
        if agent::AgentRunRequest::decode(field).is_ok() {
            output.extend_from_slice(field);
        } else {
            removed_fields.push(tag);
        }
    }
    (output, removed_fields, truncated_tail)
}

fn replace_delimited_field(
    payload: &[u8],
    target_tag: u32,
    replacement: &[u8],
) -> Result<Option<Vec<u8>>> {
    let mut remaining = payload;
    let mut output = Vec::with_capacity(payload.len() + replacement.len());
    let mut replaced = false;
    while remaining.has_remaining() {
        let start = payload.len() - remaining.remaining();
        let (tag, wire_type) = encoding::decode_key(&mut remaining)?;
        if tag == target_tag && wire_type == encoding::WireType::LengthDelimited {
            let length = encoding::decode_varint(&mut remaining)?;
            let length = usize::try_from(length)
                .map_err(|_| Error::Protocol("protobuf field length is too large".into()))?;
            if length > remaining.remaining() {
                return Err(Error::Protocol("truncated protobuf field".into()));
            }
            remaining.advance(length);
            encoding::encode_key(tag, wire_type, &mut output);
            encoding::encode_varint(replacement.len() as u64, &mut output);
            output.extend_from_slice(replacement);
            replaced = true;
        } else {
            encoding::skip_field(
                wire_type,
                tag,
                &mut remaining,
                encoding::DecodeContext::default(),
            )?;
            let end = payload.len() - remaining.remaining();
            output.extend_from_slice(&payload[start..end]);
        }
    }
    Ok(replaced.then_some(output))
}

pub async fn append(
    registry: &TransportRegistry,
    request: DecodedAppend,
    parent: Option<TransportParent>,
) -> Result<ai::BidiAppendResponse> {
    let replace_closing = request.model_id().is_some();
    let handle = registry
        .get_or_create_for_append(&request.request_id, replace_closing)
        .await?;
    let _admission = handle.admit()?;
    if let Some(conversation_id) = request.conversation_id() {
        handle.set_conversation_id(conversation_id)?;
    }
    if let Some(parent) = parent {
        handle.set_parent(parent)?;
    }
    if matches!(
        request.message.message.as_ref(),
        Some(agent::agent_client_message::Message::ClientHeartbeat(_))
    ) {
        handle.emit(&events::heartbeat())?;
    }
    handle
        .command(TransportCommand::Append {
            seqno: request.seqno,
            message: Box::new(request.message),
        })
        .await?;
    Ok(ai::BidiAppendResponse {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn append_varint_field(output: &mut Vec<u8>, tag: u32, value: u64) {
        prost::encoding::encode_key(tag, prost::encoding::WireType::Varint, output);
        prost::encoding::encode_varint(value, output);
    }

    fn append_bytes_field(output: &mut Vec<u8>, tag: u32, value: &[u8]) {
        prost::encoding::encode_key(tag, prost::encoding::WireType::LengthDelimited, output);
        prost::encoding::encode_varint(value.len() as u64, output);
        output.extend_from_slice(value);
    }

    fn request_with_varint_summary(model_id: &str) -> ai::BidiAppendRequest {
        let mut conversation_state = Vec::new();
        append_bytes_field(&mut conversation_state, 1, b"root-message-id");
        append_varint_field(&mut conversation_state, 6, 1);
        append_varint_field(&mut conversation_state, 10, agent::AgentMode::Agent as u64);

        let mut run_request = Vec::new();
        append_bytes_field(&mut run_request, 1, &conversation_state);
        append_bytes_field(
            &mut run_request,
            9,
            &agent::RequestedModel {
                model_id: model_id.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut client_message = Vec::new();
        append_bytes_field(&mut client_message, 1, &run_request);

        ai::BidiAppendRequest {
            data_binary: client_message,
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-varint-summary".into(),
            }),
            ..Default::default()
        }
    }

    fn request_with_truncated_summary(model_id: &str) -> ai::BidiAppendRequest {
        let mut conversation_state = Vec::new();
        append_bytes_field(&mut conversation_state, 1, b"root-message-id");
        prost::encoding::encode_key(
            6,
            prost::encoding::WireType::LengthDelimited,
            &mut conversation_state,
        );
        prost::encoding::encode_varint(5, &mut conversation_state);
        conversation_state.extend_from_slice(&[1, 2]);

        let mut run_request = Vec::new();
        append_bytes_field(&mut run_request, 1, &conversation_state);
        append_bytes_field(
            &mut run_request,
            9,
            &agent::RequestedModel {
                model_id: model_id.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut client_message = Vec::new();
        append_bytes_field(&mut client_message, 1, &run_request);

        ai::BidiAppendRequest {
            data_binary: client_message,
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-truncated-summary".into(),
            }),
            ..Default::default()
        }
    }

    fn request_with_malformed_file_state(model_id: &str) -> ai::BidiAppendRequest {
        let mut file_state_entry = Vec::new();
        append_bytes_field(&mut file_state_entry, 1, b"file");
        append_bytes_field(&mut file_state_entry, 2, &[0x0a, 0x05, 0x01]);

        let mut conversation_state = Vec::new();
        append_bytes_field(&mut conversation_state, 1, b"root-message-id");
        append_bytes_field(&mut conversation_state, 15, &file_state_entry);
        append_varint_field(&mut conversation_state, 10, agent::AgentMode::Agent as u64);

        let mut run_request = Vec::new();
        append_bytes_field(&mut run_request, 1, &conversation_state);
        append_bytes_field(
            &mut run_request,
            9,
            &agent::RequestedModel {
                model_id: model_id.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut client_message = Vec::new();
        append_bytes_field(&mut client_message, 1, &run_request);

        ai::BidiAppendRequest {
            data_binary: client_message,
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-malformed-file-state".into(),
            }),
            ..Default::default()
        }
    }

    fn request_with_malformed_mcp_tool(model_id: &str) -> ai::BidiAppendRequest {
        let mut conversation_state = Vec::new();
        append_bytes_field(&mut conversation_state, 1, b"root-message-id");
        append_varint_field(&mut conversation_state, 10, agent::AgentMode::Agent as u64);

        let mut mcp_tools = Vec::new();
        append_varint_field(&mut mcp_tools, 1, 7);
        append_bytes_field(
            &mut mcp_tools,
            1,
            &agent::McpToolDefinition {
                name: "valid-tool".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut run_request = Vec::new();
        append_bytes_field(&mut run_request, 1, &conversation_state);
        append_bytes_field(&mut run_request, 4, &mcp_tools);
        append_bytes_field(
            &mut run_request,
            9,
            &agent::RequestedModel {
                model_id: model_id.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut client_message = Vec::new();
        append_bytes_field(&mut client_message, 1, &run_request);

        ai::BidiAppendRequest {
            data_binary: client_message,
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-malformed-mcp-tool".into(),
            }),
            ..Default::default()
        }
    }

    fn request_with_malformed_skill_descriptor(model_id: &str) -> ai::BidiAppendRequest {
        let mut conversation_state = Vec::new();
        append_bytes_field(&mut conversation_state, 1, b"root-message-id");
        append_varint_field(&mut conversation_state, 10, agent::AgentMode::Agent as u64);

        let mut skill_options = Vec::new();
        let mut malformed_descriptor = Vec::new();
        prost::encoding::encode_key(
            1,
            prost::encoding::WireType::EndGroup,
            &mut malformed_descriptor,
        );
        append_bytes_field(&mut skill_options, 1, &malformed_descriptor);
        append_bytes_field(
            &mut skill_options,
            1,
            &agent::SkillDescriptor {
                name: "valid-skill".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut run_request = Vec::new();
        append_bytes_field(&mut run_request, 1, &conversation_state);
        append_bytes_field(&mut run_request, 7, &skill_options);
        append_bytes_field(
            &mut run_request,
            9,
            &agent::RequestedModel {
                model_id: model_id.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut client_message = Vec::new();
        append_bytes_field(&mut client_message, 1, &run_request);

        ai::BidiAppendRequest {
            data_binary: client_message,
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-malformed-skill-descriptor".into(),
            }),
            ..Default::default()
        }
    }

    fn request_with_multiple_malformed_optional_fields(model_id: &str) -> ai::BidiAppendRequest {
        let mut conversation_state = Vec::new();
        append_bytes_field(&mut conversation_state, 1, b"root-message-id");
        append_varint_field(&mut conversation_state, 6, 1);
        append_bytes_field(&mut conversation_state, 12, &[0x0a, 0x05, 0x01]);
        append_varint_field(&mut conversation_state, 7, 1);
        append_bytes_field(&mut conversation_state, 12, &[0x0a, 0x05, 0x01]);
        append_varint_field(&mut conversation_state, 10, agent::AgentMode::Agent as u64);
        prost::encoding::encode_key(
            63,
            prost::encoding::WireType::LengthDelimited,
            &mut conversation_state,
        );
        prost::encoding::encode_varint(5, &mut conversation_state);
        conversation_state.extend_from_slice(&[1, 2]);

        let mut mcp_tools = Vec::new();
        append_varint_field(&mut mcp_tools, 1, 7);
        append_bytes_field(
            &mut mcp_tools,
            1,
            &agent::McpToolDefinition {
                name: "valid-tool".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut skill_options = Vec::new();
        let mut malformed_descriptor = Vec::new();
        prost::encoding::encode_key(
            1,
            prost::encoding::WireType::EndGroup,
            &mut malformed_descriptor,
        );
        append_bytes_field(&mut skill_options, 1, &malformed_descriptor);
        append_bytes_field(
            &mut skill_options,
            1,
            &agent::SkillDescriptor {
                name: "valid-skill".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut run_request = Vec::new();
        append_bytes_field(&mut run_request, 1, &conversation_state);
        append_bytes_field(&mut run_request, 4, &mcp_tools);
        append_bytes_field(&mut run_request, 7, &skill_options);
        append_bytes_field(
            &mut run_request,
            9,
            &agent::RequestedModel {
                model_id: model_id.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let mut client_message = Vec::new();
        append_bytes_field(&mut client_message, 1, &run_request);

        ai::BidiAppendRequest {
            data_binary: client_message,
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-multiple-malformed-fields".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn decodes_agent_client_message_from_binary_data() {
        let payload = agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::ClientHeartbeat(
                agent::ClientHeartbeat {},
            )),
        }
        .encode_to_vec();
        let request = ai::BidiAppendRequest {
            data_binary: payload,
            request_id: Some(ai::BidiRequestId {
                request_id: "binary-request".into(),
            }),
            ..Default::default()
        };

        let decoded = decode(&request).expect("binary AgentClientMessage should decode");

        assert_eq!(decoded.request_id, "binary-request");
        assert!(matches!(
            decoded.message.message,
            Some(agent::agent_client_message::Message::ClientHeartbeat(_))
        ));
    }

    #[test]
    fn routing_model_id_does_not_decode_conversation_state() {
        let request = request_with_varint_summary("custom-model");

        let route = route_info(&request).expect("model routing should ignore checkpoint details");

        assert_eq!(route.request_id, "request-with-varint-summary");
        assert_eq!(route.model_id.as_deref(), Some("custom-model"));
        assert!(route.is_run_request);
    }

    #[test]
    fn decode_skips_varint_summary_and_preserves_other_checkpoint_fields() {
        let request = request_with_varint_summary("custom-model");

        let decoded = decode(&request).expect("malformed summary metadata should not reject a run");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };
        let state = run_request
            .conversation_state
            .expect("conversation state should be retained");

        assert_eq!(state.root_prompt_messages_json, [b"root-message-id"]);
        assert_eq!(state.mode, Some(agent::AgentMode::Agent as i32));
        assert_eq!(state.summary, None);
        assert_eq!(
            run_request.requested_model.unwrap().model_id,
            "custom-model"
        );
    }

    #[test]
    fn decode_skips_truncated_checkpoint_field_and_preserves_run_request() {
        let request = request_with_truncated_summary("custom-model");

        let decoded = decode(&request).expect("malformed checkpoint summary should be discarded");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };
        let state = run_request
            .conversation_state
            .expect("conversation state should be retained");

        assert_eq!(state.root_prompt_messages_json, [b"root-message-id"]);
        assert_eq!(state.summary, None);
        assert_eq!(
            run_request.requested_model.unwrap().model_id,
            "custom-model"
        );
    }

    #[test]
    fn decode_skips_malformed_nested_checkpoint_field_and_keeps_later_fields() {
        let request = request_with_malformed_file_state("custom-model");

        let decoded = decode(&request).expect("malformed nested checkpoint should be discarded");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };
        let state = run_request
            .conversation_state
            .expect("conversation state should be retained");

        assert_eq!(state.root_prompt_messages_json, [b"root-message-id"]);
        assert_eq!(state.mode, Some(agent::AgentMode::Agent as i32));
        assert!(state.file_states_v2.is_empty());
        assert_eq!(
            run_request.requested_model.unwrap().model_id,
            "custom-model"
        );
    }

    #[test]
    fn decode_skips_malformed_mcp_tool_and_preserves_run_fields() {
        let request = request_with_malformed_mcp_tool("custom-model");

        let decoded = decode(&request).expect("malformed MCP tool entry should be discarded");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };

        let mcp_tools = run_request.mcp_tools.unwrap();
        assert_eq!(mcp_tools.mcp_tools.len(), 1);
        assert_eq!(mcp_tools.mcp_tools[0].name, "valid-tool");
        assert_eq!(
            run_request
                .conversation_state
                .unwrap()
                .root_prompt_messages_json,
            [b"root-message-id"]
        );
        assert_eq!(
            run_request.requested_model.unwrap().model_id,
            "custom-model"
        );
    }

    #[test]
    fn decode_skips_malformed_skill_descriptor_and_preserves_valid_skill() {
        let request = request_with_malformed_skill_descriptor("custom-model");

        let decoded = decode(&request).expect("malformed skill descriptor should be discarded");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };
        let skill_options = run_request
            .skill_options
            .expect("valid skill options should be retained");

        assert_eq!(skill_options.skill_descriptors.len(), 1);
        assert_eq!(skill_options.skill_descriptors[0].name, "valid-skill");
        assert_eq!(
            run_request.requested_model.unwrap().model_id,
            "custom-model"
        );
    }

    #[test]
    fn decode_skips_multiple_malformed_optional_fields_and_preserves_valid_run_data() {
        let request = request_with_multiple_malformed_optional_fields("custom-model");

        let decoded = decode(&request).expect("unrelated malformed fields should not reject a run");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };

        let state = run_request
            .conversation_state
            .expect("valid checkpoint fields should be retained");
        assert_eq!(state.root_prompt_messages_json, [b"root-message-id"]);
        assert_eq!(state.mode, Some(agent::AgentMode::Agent as i32));
        assert_eq!(state.summary, None);
        assert!(state.file_states.is_empty());

        let mcp_tools = run_request
            .mcp_tools
            .expect("valid MCP tools should be retained");
        assert_eq!(mcp_tools.mcp_tools.len(), 1);
        assert_eq!(mcp_tools.mcp_tools[0].name, "valid-tool");

        let skill_options = run_request
            .skill_options
            .expect("valid skill descriptors should be retained");
        assert_eq!(skill_options.skill_descriptors.len(), 1);
        assert_eq!(skill_options.skill_descriptors[0].name, "valid-skill");
        assert_eq!(
            run_request.requested_model.unwrap().model_id,
            "custom-model"
        );
    }

    #[test]
    fn decode_preserves_length_delimited_summary() {
        let request = ai::BidiAppendRequest {
            data_binary: agent::AgentClientMessage {
                message: Some(agent::agent_client_message::Message::RunRequest(
                    agent::AgentRunRequest {
                        conversation_state: Some(agent::ConversationStateStructure {
                            summary: Some(vec![1, 2, 3]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            }
            .encode_to_vec(),
            request_id: Some(ai::BidiRequestId {
                request_id: "request-with-bytes-summary".into(),
            }),
            ..Default::default()
        };

        let decoded = decode(&request).expect("valid summary bytes should decode");
        let Some(agent::agent_client_message::Message::RunRequest(run_request)) =
            decoded.message.message
        else {
            panic!("expected AgentRunRequest");
        };

        assert_eq!(
            run_request.conversation_state.unwrap().summary,
            Some(vec![1, 2, 3])
        );
    }
}
