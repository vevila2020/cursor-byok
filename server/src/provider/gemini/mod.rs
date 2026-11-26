//! Gemini GenerateContent streaming adapter.
mod request;
#[cfg(test)]
mod tests;

use async_stream::try_stream;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{
    attempt::{send_once, Attempt},
    map_sse_error, provider_event_error,
    recorder::recorded_headers,
    CallRecorder, FinishReason, ModelEvent, Provider, ProviderStream,
};
use crate::{
    config::ProviderConfig,
    model::{ModelInvocation, ProviderReplayState, Usage},
    Error, Result,
};

pub(super) struct GeminiProvider {
    client: reqwest::Client,
    config: ProviderConfig,
    recorder: Option<CallRecorder>,
}

impl GeminiProvider {
    pub(super) fn new(
        client: reqwest::Client,
        config: ProviderConfig,
        recorder: Option<CallRecorder>,
    ) -> Self {
        Self {
            client,
            config,
            recorder,
        }
    }
}

pub(super) fn usage(value: &Value) -> Usage {
    let tokens = |key: &str| value[key].as_u64();
    Usage {
        input_tokens: tokens("promptTokenCount"),
        context_input_tokens: tokens("promptTokenCount"),
        output_tokens: tokens("candidatesTokenCount"),
        total_tokens: tokens("totalTokenCount"),
        cache_read_tokens: tokens("cachedContentTokenCount"),
        cache_write_tokens: None,
        reasoning_tokens: tokens("thoughtsTokenCount"),
    }
}

#[derive(Default)]
struct Response {
    parts: Vec<Value>,
    thought_block: Option<bool>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl Response {
    fn chunk(&mut self, chunk: Value) -> Result<Vec<ModelEvent>> {
        if let Some(error) = provider_event_error("Gemini", &chunk) {
            return Err(error);
        }
        if let Some(block) = chunk
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            return Err(Error::Provider(format!("Gemini blocked prompt: {block}")));
        }
        if let Some(metadata) = chunk.get("usageMetadata") {
            self.usage = Some(usage(metadata));
        }
        let mut events = Vec::new();
        let Some(candidate) = chunk.pointer("/candidates/0") else {
            return Ok(events);
        };
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part["text"].as_str().filter(|t| !t.is_empty()) {
                    let thought = part["thought"].as_bool().unwrap_or(false);
                    if self.thought_block != Some(thought) {
                        self.close_block(&mut events);
                        events.push(if thought {
                            ModelEvent::ThinkingStart
                        } else {
                            ModelEvent::TextStart
                        });
                        self.thought_block = Some(thought);
                    }
                    events.push(if thought {
                        ModelEvent::ThinkingDelta(text.into())
                    } else {
                        ModelEvent::TextDelta(text.into())
                    });
                }
                self.parts.push(part.clone());
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(events)
    }

    fn close_block(&mut self, events: &mut Vec<ModelEvent>) {
        if let Some(thought) = self.thought_block.take() {
            events.push(if thought {
                ModelEvent::ThinkingEnd
            } else {
                ModelEvent::TextEnd
            });
        }
    }

    fn finish(mut self, model: &str) -> Result<Vec<ModelEvent>> {
        let mut events = Vec::new();
        self.close_block(&mut events);
        let mut reason = match self.finish_reason.as_deref() {
            Some("STOP") => FinishReason::Stop,
            Some("MAX_TOKENS") => FinishReason::Length,
            Some(reason) => {
                return Err(Error::Provider(format!(
                    "Gemini stopped generation: {reason}"
                )))
            }
            None => {
                return Err(Error::Provider(
                    "Gemini stream ended without finishReason".into(),
                ))
            }
        };
        let mut ids = Vec::new();
        if reason == FinishReason::Stop {
            for (index, call) in self
                .parts
                .iter()
                .filter_map(|p| p.get("functionCall"))
                .enumerate()
            {
                let name = call["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::Protocol("Gemini functionCall missing name".into()))?;
                let id = call["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("gemini-{}", uuid::Uuid::new_v4()));
                if ids.contains(&id) {
                    return Err(Error::Protocol("Gemini repeated a tool call ID".into()));
                }
                ids.push(id.clone());
                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                if !args.is_object() {
                    return Err(Error::Protocol(
                        "Gemini functionCall args must be an object".into(),
                    ));
                }
                events.extend([
                    ModelEvent::ToolCallStart {
                        index,
                        call_id: id,
                        name: name.into(),
                    },
                    ModelEvent::ToolCallArgumentsDelta {
                        index,
                        delta: args.to_string(),
                    },
                    ModelEvent::ToolCallEnd { index },
                ]);
                reason = FinishReason::ToolUse;
            }
        }
        if !ids.is_empty()
            || self
                .parts
                .iter()
                .any(|p| p.get("thoughtSignature").is_some())
        {
            events.push(ModelEvent::ProviderReplayState(ProviderReplayState {
                provider_kind: "gemini".into(),
                value: json!({"model":model,"parts":self.parts,"callIds":ids}),
            }));
        }
        if let Some(value) = self.usage {
            events.push(ModelEvent::Usage(value));
        }
        events.push(ModelEvent::Done(reason));
        Ok(events)
    }
}

impl Provider for GeminiProvider {
    fn stream(
        &self,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        let client = self.client.clone();
        let config = self.config.clone();
        let recorder = self.recorder.clone();
        Box::pin(try_stream! {
            let body = request::body(&invocation.request,config.max_output_tokens)?;
            if let Some(recorder) = &recorder {
                recorder.request(recorded_headers(&config,&[("content-type","application/json")]), &body).await?;
            }
            let attempt = send_once("Gemini", || client.post(&config.request_url).header("x-goog-api-key",&config.api_key).headers(config.custom_headers.clone()).json(&body), &cancellation, recorder.as_ref()).await?;
            let Attempt::Response(response) = attempt else {return};
            yield ModelEvent::Start{model_call_id:invocation.call_id};
            let bytes = response.bytes_stream().then(move |chunk| {
                let recorder = recorder.clone();
                async move {let bytes = chunk?; if let Some(recorder) = recorder {recorder.response_chunk(&bytes).await?;} Ok::<_,Error>(bytes)}
            });
            let stream = bytes.eventsource();
            futures_util::pin_mut!(stream);
            let mut state = Response::default();
            loop {
                let item = tokio::select! { _ = cancellation.cancelled() => return, item = stream.next() => item };
                let Some(event) = item else {break};
                let event = event.map_err(|e| map_sse_error("Gemini",e))?;
                for event in state.chunk(serde_json::from_str(&event.data)?)? {yield event;}
            }
            for event in state.finish(&invocation.request.model.model_id)? {yield event;}
        })
    }
}
