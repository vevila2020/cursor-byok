use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use crate::model::{
    ContentPart, ModelRequest, ProjectedContent, Role, ToolCallContent, ToolResultContent,
};
use crate::{Error, Result};

fn part_value(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({"text": text}),
        ContentPart::Image { mime_type, data } => {
            json!({"inlineData": {"mimeType":mime_type,"data":STANDARD.encode(data)}})
        }
    }
}

fn tool_response(result: &ToolResultContent, call: &Value) -> Result<Value> {
    let function = call
        .get("functionCall")
        .ok_or_else(|| Error::Protocol("missing Gemini functionCall in history".into()))?;
    let content = if result.is_error {
        json!({"error": result.content})
    } else {
        json!({"output": result.content})
    };
    let mut response = json!({"name": function["name"], "response": content});
    if let Some(id) = function.get("id") {
        response["id"] = id.clone();
    }
    let images: Vec<_> = result
        .provider_parts
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .map(part_value)
        .collect();
    if !images.is_empty() {
        response["parts"] = json!(images);
    }
    Ok(json!({"functionResponse": response}))
}

fn assistant_parts(text: &str, calls: &[ToolCallContent]) -> Vec<Value> {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"text":text}));
    }
    parts.extend(
        calls
            .iter()
            .map(|c| json!({"functionCall":{"name":c.name,"args":c.arguments}})),
    );
    parts
}

pub(super) fn body(request: &ModelRequest, max_output: Option<u64>) -> Result<Value> {
    let mut system = Vec::new();
    if !request.prompt.instructions.is_empty() {
        system.push(json!({"text":request.prompt.instructions}));
    }
    let mut contents = Vec::new();
    let mut history = request.history.iter().peekable();
    while let Some(message) = history.next() {
        match &message.content {
            ProjectedContent::Parts(parts) => {
                let parts: Vec<_> = parts.iter().map(part_value).collect();
                if message.role == Role::System {
                    system.extend(parts);
                } else if !parts.is_empty() {
                    let role = if message.role == Role::Assistant {
                        "model"
                    } else {
                        "user"
                    };
                    contents.push(json!({"role":role,"parts":parts}));
                }
            }
            ProjectedContent::Assistant {
                text,
                calls,
                replay_state,
                ..
            } => {
                let replay = replay_state.as_ref().filter(|state| {
                    state.provider_kind == "gemini"
                        && state.value["model"] == request.model.model_id
                });
                let parts = replay
                    .and_then(|s| s.value["parts"].as_array())
                    .cloned()
                    .unwrap_or_else(|| assistant_parts(text, calls));
                if parts.is_empty() {
                    continue;
                }
                let functions: Vec<_> = parts
                    .iter()
                    .filter(|p| p.get("functionCall").is_some())
                    .collect();
                if functions.len() != calls.len() {
                    return Err(Error::Protocol(
                        "Gemini replay does not match tool calls".into(),
                    ));
                }
                let mut results = std::collections::HashMap::new();
                while let Some(result) = history.peek().and_then(|msg| match &msg.content {
                    ProjectedContent::ToolResult(r) => Some(r),
                    _ => None,
                }) {
                    if results.insert(result.call_id.as_str(), result).is_some() {
                        return Err(Error::Protocol("duplicate Gemini tool response".into()));
                    }
                    history.next();
                }
                let mut responses = Vec::new();
                for (index, (call, function)) in calls.iter().zip(functions).enumerate() {
                    // Signed parts retain the original upstream ordering, even when the client
                    // returns parallel calls and results in a different order.
                    let call_id = if let Some(state) = replay {
                        state.value["callIds"][index].as_str().ok_or_else(|| {
                            Error::Protocol("Gemini replay missing call ID".into())
                        })?
                    } else {
                        &call.call_id
                    };
                    let result = results.remove(call_id).ok_or_else(|| {
                        Error::Protocol(format!("missing Gemini tool response for {}", call_id))
                    })?;
                    responses.push(tool_response(result, function)?);
                }
                if !results.is_empty() {
                    return Err(Error::Protocol("unmatched Gemini tool responses".into()));
                }
                contents.push(json!({"role":"model","parts":parts}));
                if !responses.is_empty() {
                    contents.push(json!({"role":"user","parts":responses}));
                }
            }
            ProjectedContent::ToolResult(_) => {
                return Err(Error::Protocol(
                    "Gemini tool result without a model call".into(),
                ))
            }
        }
    }
    let mut body = json!({"contents":contents});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if !request.prompt.tools.is_empty() {
        let declarations: Vec<_> = request.prompt.tools.iter().map(|tool| json!({
            "name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters
        })).collect();
        body["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    if let Some(max) = request.model.max_output_tokens.or(max_output) {
        body["generationConfig"] = json!({"maxOutputTokens":max});
    }
    Ok(body)
}
