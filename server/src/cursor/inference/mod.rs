//! Serves Cursor's local agent loop over `aiserver.v1.InferenceService`.
//!
//! Cursor 3.31's agent host runs the agent loop locally and asks the backend
//! for model invocations through `RunInference`. These appends share the run's
//! request ID with `BidiService/BidiAppend`, so [`classify`] tells the two
//! message families apart before routing.

mod convert;
mod model;
mod session;
pub mod stream;
pub mod wire;

use prost::Message;

pub use session::{InferenceHandle, InferenceRegistry, InferenceRoute};

use crate::cursor::protocol::proto::agent::v1 as agent;

pub enum AppendClass {
    Agent,
    Inference(Box<wire::RunInferenceClientMessage>),
}

/// Routes one `BidiAppend` payload to the agent run channel or the local-loop
/// inference channel. `inference_open` reports whether this request ID already
/// has a `RunInference` stream, which breaks ties for shared field numbers.
pub fn classify(data: &[u8], inference_open: bool) -> AppendClass {
    let inference = wire::RunInferenceClientMessage::decode(data)
        .ok()
        .filter(inference_plausible);
    let agent = agent::AgentClientMessage::decode(data)
        .ok()
        .filter(agent_plausible);
    match (inference, agent) {
        (Some(inference), None) => AppendClass::Inference(Box::new(inference)),
        (None, Some(_)) | (None, None) => AppendClass::Agent,
        (Some(inference), Some(agent)) => match arbitrate(&inference, &agent) {
            Some(true) => AppendClass::Inference(Box::new(inference)),
            Some(false) => AppendClass::Agent,
            None if inference_open => AppendClass::Inference(Box::new(inference)),
            None => AppendClass::Agent,
        },
    }
}

fn arbitrate(
    inference: &wire::RunInferenceClientMessage,
    agent: &agent::AgentClientMessage,
) -> Option<bool> {
    use agent::agent_client_message::Message as AgentMessage;
    use wire::run_inference_client_message::Message as InferenceMessage;
    match (&inference.message, &agent.message) {
        (Some(InferenceMessage::RunRequest(inference)), Some(AgentMessage::RunRequest(agent))) => {
            // An agent run request's conversation_state is a protobuf message;
            // when its bytes are coincidentally valid UTF-8 the payload also
            // decodes as a RunInference request. Only a UUID-shaped inner
            // conversation id is a real RunInference run request.
            if looks_like_conversation_id(&inference.conversation_id) {
                return Some(true);
            }
            if agent.conversation_state.is_some() || agent.action.is_some() {
                return Some(false);
            }
            inference.requested_model.is_some().then_some(true)
        }
        // `cancel_invocation` and `kv_client_message` share field 3. Prefer the
        // agent channel so blob transfers are never dropped; a missed cancel is
        // recovered by the stream close that follows it.
        (Some(InferenceMessage::CancelInvocation(_)), Some(_)) => Some(false),
        _ => None,
    }
}

fn inference_plausible(message: &wire::RunInferenceClientMessage) -> bool {
    use wire::run_inference_client_message::Message;
    match message.message.as_ref() {
        Some(Message::RunRequest(request)) => looks_like_conversation_id(&request.conversation_id),
        Some(Message::InvokeModel(invocation)) => {
            !invocation.invocation_id.is_empty() && invocation.request.is_some()
        }
        Some(Message::CancelInvocation(cancel)) => !cancel.invocation_id.is_empty(),
        Some(Message::FinishRun(_)) => true,
        Some(Message::ClientHeartbeat(_)) => true,
        Some(Message::AgentEvent(event)) => !event.method_name.is_empty(),
        Some(Message::RunRequestLog(log)) => !log.agent_run_request.is_empty(),
        Some(Message::ResolveSubagentModels(resolve)) => !resolve.resolution_id.is_empty(),
        Some(Message::ClientFirstCallTimings(timings)) => !timings.invocation_id.is_empty(),
        Some(Message::DeltaBaseSeed(seed)) => !seed.invocation_id.is_empty(),
        None => false,
    }
}

fn looks_like_conversation_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (16..=64).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
        && bytes.iter().filter(|byte| **byte == b'-').count() >= 2
}

fn agent_plausible(message: &agent::AgentClientMessage) -> bool {
    use agent::agent_client_message::Message;
    match message.message.as_ref() {
        Some(Message::RunRequest(request)) => {
            request.conversation_state.is_some() || request.action.is_some()
        }
        Some(Message::ExecClientMessage(message)) => message.id != 0 && message.message.is_some(),
        Some(Message::ExecClientControlMessage(message)) => message.message.is_some(),
        Some(Message::KvClientMessage(message)) => message.message.is_some(),
        Some(Message::ConversationAction(action)) => action.action.is_some(),
        Some(Message::InteractionResponse(response)) => response.id != 0,
        Some(Message::ClientHeartbeat(_) | Message::PrewarmRequest(_)) => true,
        None => false,
    }
}

pub fn run_request_conversation(message: &wire::RunInferenceClientMessage) -> Option<&str> {
    match message.message.as_ref()? {
        wire::run_inference_client_message::Message::RunRequest(request) => {
            Some(request.conversation_id.as_str()).filter(|id| !id.is_empty())
        }
        _ => None,
    }
}

pub fn run_request_model(message: &wire::RunInferenceClientMessage) -> Option<&str> {
    match message.message.as_ref()? {
        wire::run_inference_client_message::Message::RunRequest(request) => request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty()),
        _ => None,
    }
}

/// Older transports send `BidiAppendRequest.data` as hex instead of
/// `data_binary`. Both carry the same serialized message.
pub fn decode_hex(text: &str) -> crate::Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return Err(crate::Error::Protocol(
            "BidiAppend hex payload has an odd length".into(),
        ));
    }
    let mut output = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for chunk in bytes.as_chunks::<2>().0 {
        let high = hex_digit(chunk[0])?;
        let low = hex_digit(chunk[1])?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

fn hex_digit(byte: u8) -> crate::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(crate::Error::Protocol(
            "BidiAppend hex payload has a non-hex digit".into(),
        )),
    }
}

pub fn is_turn_completed(message: &wire::RunInferenceClientMessage) -> bool {
    matches!(
        message.message.as_ref(),
        Some(wire::run_inference_client_message::Message::FinishRun(run))
            if !run.aborted && run.turn_completed.unwrap_or(false)
    )
}

pub fn message_kind(message: &wire::RunInferenceClientMessage) -> &'static str {
    use wire::run_inference_client_message::Message;
    match message.message.as_ref() {
        Some(Message::RunRequest(_)) => "run_request",
        Some(Message::InvokeModel(_)) => "invoke_model",
        Some(Message::CancelInvocation(_)) => "cancel_invocation",
        Some(Message::FinishRun(_)) => "finish_run",
        Some(Message::ClientHeartbeat(_)) => "client_heartbeat",
        Some(Message::AgentEvent(_)) => "agent_event",
        Some(Message::RunRequestLog(_)) => "run_request_log",
        Some(Message::ResolveSubagentModels(_)) => "resolve_subagent_models",
        Some(Message::ClientFirstCallTimings(_)) => "client_first_call_timings",
        Some(Message::DeltaBaseSeed(_)) => "delta_base_seed",
        None => "empty",
    }
}

pub async fn is_local_model(store: &crate::store::Store, model_id: &str) -> crate::Result<bool> {
    Ok(model_id.starts_with(crate::plugin::ADAPTER_ID_PREFIX)
        || store.model(model_id).await?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inference_run_request() -> wire::RunInferenceClientMessage {
        wire::RunInferenceClientMessage {
            message: Some(wire::run_inference_client_message::Message::RunRequest(
                wire::RunInferenceRunRequest {
                    conversation_id: "49d70f87-629a-4f22-8214-36fc6a57ea41".into(),
                    requested_model: Some(wire::InferenceRequestedModel {
                        model_id: "byok-model".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }
    }

    #[test]
    fn run_inference_heartbeat_is_not_an_agent_control_message() {
        let payload = wire::RunInferenceClientMessage {
            message: Some(
                wire::run_inference_client_message::Message::ClientHeartbeat(
                    wire::RunInferenceClientHeartbeat::default(),
                ),
            ),
        }
        .encode_to_vec();

        assert!(matches!(
            classify(&payload, true),
            AppendClass::Inference(_)
        ));
    }

    #[test]
    fn empty_agent_heartbeat_stays_on_the_agent_channel() {
        let payload = agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::ClientHeartbeat(
                agent::ClientHeartbeat::default(),
            )),
        }
        .encode_to_vec();

        assert!(matches!(classify(&payload, true), AppendClass::Agent));
    }

    #[test]
    fn inference_run_request_wins_when_the_agent_shape_is_absent() {
        let payload = inference_run_request().encode_to_vec();

        let AppendClass::Inference(message) = classify(&payload, false) else {
            panic!("expected RunInference run_request");
        };
        assert_eq!(run_request_model(&message), Some("byok-model"));
    }

    #[test]
    fn agent_run_request_stays_on_the_agent_channel() {
        let payload = agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::RunRequest(
                agent::AgentRunRequest {
                    conversation_state: Some(agent::ConversationStateStructure::default()),
                    action: Some(agent::ConversationAction {
                        action: Some(agent::conversation_action::Action::UserMessageAction(
                            agent::UserMessageAction::default(),
                        )),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }
        .encode_to_vec();

        assert!(matches!(classify(&payload, true), AppendClass::Agent));
    }

    #[test]
    fn hex_appends_decode_to_the_same_bytes() {
        let payload = inference_run_request().encode_to_vec();
        let hex = payload
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        assert_eq!(decode_hex(&hex).unwrap(), payload);
        assert!(matches!(
            classify(&decode_hex(&hex).unwrap(), true),
            AppendClass::Inference(_)
        ));
    }

    #[test]
    fn finish_run_does_not_decode_as_a_conversation_action() {
        let payload = wire::RunInferenceClientMessage {
            message: Some(wire::run_inference_client_message::Message::FinishRun(
                wire::RunInferenceFinishRun {
                    aborted: false,
                    turn_completed: Some(true),
                },
            )),
        }
        .encode_to_vec();

        assert!(matches!(
            classify(&payload, false),
            AppendClass::Inference(_)
        ));
    }
}
