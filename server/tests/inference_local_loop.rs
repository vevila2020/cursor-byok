//! Verifies the RunInference bridge used by Cursor's local agent loop.
#![allow(dead_code)]

#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{sync::Arc, time::Duration};

use cursor_server::{
    cursor::{
        inference::{wire::*, InferenceRegistry},
        protocol::{connect, proto::aiserver::v1 as ai},
        services::observability::CursorTraceService,
    },
    model::{ModelConfigInput, ModelType, OPENAI_CHAT_ENDPOINT},
    provider::ModelEvent,
};
use prost::Message;
use serde_json::json;

fn model_input() -> ModelConfigInput {
    ModelConfigInput {
        sort_order: 0,
        display_name: "Local Loop".into(),
        group_name: None,
        model_type: ModelType::OpenAi,
        base_url: "https://example.com/v1".into(),
        use_full_url: false,
        api_key: "upstream".into(),
        tooltip_data: "test".into(),
        model_id: "qwen/model".into(),
        reasoning_effort: None,
        openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
        openai_extra_params_enabled: false,
        openai_extra_params: json!({}),
        custom_headers_enabled: false,
        custom_headers: json!({}),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: json!({}),
        context_window_tokens: None,
        max_completion_tokens: None,
        anthropic_max_tokens: None,
        anthropic_thinking_effort: None,
        thinking_budget_tokens: None,
    }
}

fn run_request(model_id: &str) -> RunInferenceClientMessage {
    RunInferenceClientMessage {
        message: Some(run_inference_client_message::Message::RunRequest(
            RunInferenceRunRequest {
                conversation_id: "conversation-1".into(),
                requested_model: Some(InferenceRequestedModel {
                    model_id: model_id.into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    }
}

fn invoke_model(model_id: &str) -> RunInferenceClientMessage {
    RunInferenceClientMessage {
        message: Some(run_inference_client_message::Message::InvokeModel(
            RunInferenceInvokeModel {
                invocation_id: "invocation-1".into(),
                request: Some(InferenceStreamRequest {
                    messages: vec![InferenceCoreMessage {
                        role: MESSAGE_ROLE_USER,
                        content: Some(inference_core_message::Content::Text("hello".into())),
                        ..Default::default()
                    }],
                    model_id: model_id.into(),
                    conversation_id: "conversation-1".into(),
                    ..Default::default()
                }),
            },
        )),
    }
}

async fn next_message(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>,
) -> RunInferenceServerMessage {
    let frame = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("timed out waiting for an inference frame")
        .expect("inference output closed");
    let messages = connect::decode_frames(&frame).expect("Connect frames should decode");
    let (_, payload) = messages
        .into_iter()
        .find(|(flags, _)| flags & connect::END_STREAM_FLAG == 0)
        .expect("expected a payload frame");
    RunInferenceServerMessage::decode(payload.as_ref()).expect("server message should decode")
}

#[tokio::test]
async fn run_inference_session_streams_provider_events() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store.create_model(&model_input()).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::TextStart,
        ModelEvent::TextDelta("hi".into()),
        ModelEvent::TextEnd,
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: "call-1".into(),
            name: "Glob".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: r#"{"glob_pattern":"**/*"}"#.into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(cursor_server::provider::FinishReason::ToolUse),
    ]);

    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        CursorTraceService::new(store.clone()),
    );
    let handle = registry
        .ensure_local("request-1", None, None)
        .await
        .unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append("request-1", run_request(&model.model_hash))
        .await
        .unwrap();

    let ready = next_message(&mut receiver).await;
    let run_inference_server_message::Message::RunReady(ready) = ready.message.unwrap() else {
        panic!("expected run_ready");
    };
    assert_eq!(ready.resolved_model.unwrap().model_id, model.model_hash);
    let metadata = ready
        .prompt_model_metadata
        .expect("local loop requires prompt model metadata");
    assert_eq!(metadata.vendor, "openai");
    assert_eq!(metadata.prompt_version, "latest");
    assert!(metadata
        .agent_mode_configs
        .iter()
        .any(|mode| mode.mode_id == "agent"));

    registry
        .append("request-1", invoke_model(&model.model_hash))
        .await
        .unwrap();

    let mut text = String::new();
    let mut text_finished = false;
    let mut tool_started = false;
    let mut tool_delta = String::new();
    let mut tool_complete = None;
    let mut response_info = None;
    let error = loop {
        let message = next_message(&mut receiver).await;
        match message.message.unwrap() {
            run_inference_server_message::Message::InvocationResponse(response) => {
                let response = response.response.unwrap().response.unwrap();
                match response {
                    inference_stream_response::Response::TextPart(part) => {
                        if part.is_final {
                            assert!(!text_finished, "final text part must be unique");
                            text_finished = true;
                            assert!(part.text.is_empty());
                        } else {
                            text.push_str(&part.text);
                        }
                    }
                    inference_stream_response::Response::ToolCallPart(part) => {
                        assert_eq!(part.tool_call_id, "call-1");
                        if part.is_complete {
                            tool_complete = Some((part.tool_name, part.args));
                        } else if part.tool_name.is_empty() {
                            tool_delta.push_str(&part.args);
                        } else {
                            assert_eq!(part.tool_name, "Glob");
                            assert!(part.args.is_empty());
                            tool_started = true;
                        }
                    }
                    inference_stream_response::Response::ResponseInfo(info) => {
                        response_info = Some(info);
                    }
                    inference_stream_response::Response::Usage(_)
                    | inference_stream_response::Response::ExtendedUsage(_) => {}
                    other => panic!("unexpected inference response: {other:?}"),
                }
            }
            run_inference_server_message::Message::InvocationEnd(end) => {
                break end.error;
            }
            run_inference_server_message::Message::Heartbeat(_) => {}
            run_inference_server_message::Message::RunReady(_) => {
                panic!("run_ready must precede invocations")
            }
        }
    };

    assert!(error.is_none(), "invocation ended with {error:?}");
    assert!(text_finished, "invocation must end with a final text part");
    assert_eq!(text, "hi");
    assert!(tool_started, "tool call start must be streamed");
    assert_eq!(tool_delta, r#"{"glob_pattern":"**/*"}"#);
    assert_eq!(
        tool_complete,
        Some(("Glob".to_owned(), r#"{"glob_pattern":"**/*"}"#.to_owned()))
    );
    let info = response_info.expect("response_info must be streamed");
    assert_eq!(info.id, "invocation-1");
    assert_eq!(info.model, model.model_hash);
    assert_eq!(info.messages.len(), 1);
    let message = &info.messages[0];
    assert_eq!(message.role, MESSAGE_ROLE_ASSISTANT);
    assert_eq!(message.content.as_deref(), Some("hi"));
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].tool_name, "Glob");
    assert!(message.tool_calls[0].args.is_some());
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].history.len(), 1);
    assert_eq!(requests[0].prompt.instructions, "");
}

#[tokio::test]
async fn provider_replay_state_is_replayed_on_the_next_invocation() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store.create_model(&model_input()).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: "call-1".into(),
            name: "Glob".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: "{}".into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::ProviderReplayState(cursor_server::model::ProviderReplayState {
            provider_kind: "openai_responses".into(),
            value: json!({
                "items": [{
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "secret"
                }]
            }),
        }),
        ModelEvent::Done(cursor_server::provider::FinishReason::ToolUse),
    ]);
    provider.push(vec![
        ModelEvent::TextDelta("done".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(cursor_server::provider::FinishReason::Stop),
    ]);

    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        CursorTraceService::new(store.clone()),
    );
    let handle = registry
        .ensure_local("request-replay", None, None)
        .await
        .unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append("request-replay", run_request(&model.model_hash))
        .await
        .unwrap();
    let _ready = next_message(&mut receiver).await;

    registry
        .append("request-replay", invoke_model(&model.model_hash))
        .await
        .unwrap();
    drain_invocation(&mut receiver).await;

    let followup = RunInferenceClientMessage {
        message: Some(run_inference_client_message::Message::InvokeModel(
            RunInferenceInvokeModel {
                invocation_id: "invocation-2".into(),
                request: Some(InferenceStreamRequest {
                    messages: vec![
                        InferenceCoreMessage {
                            role: MESSAGE_ROLE_ASSISTANT,
                            content: Some(inference_core_message::Content::Text(String::new())),
                            tool_calls: vec![InferenceToolCall {
                                tool_call_id: "call-1".into(),
                                tool_name: "Glob".into(),
                                args: Some(::prost_types::Struct::default()),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                        InferenceCoreMessage {
                            role: MESSAGE_ROLE_TOOL,
                            content: Some(inference_core_message::Content::ToolContent(
                                InferenceToolResultContent {
                                    parts: vec![InferenceToolResultPart {
                                        tool_call_id: "call-1".into(),
                                        tool_name: "Glob".into(),
                                        ..Default::default()
                                    }],
                                },
                            )),
                            ..Default::default()
                        },
                    ],
                    model_id: model.model_hash.clone(),
                    conversation_id: "conversation-1".into(),
                    ..Default::default()
                }),
            },
        )),
    };
    registry.append("request-replay", followup).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while provider.requests().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "second invocation never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let requests = provider.requests();
    let assistant = requests[1]
        .history
        .iter()
        .find_map(|message| match &message.content {
            cursor_server::model::ProjectedContent::Assistant { replay_state, .. } => {
                Some(replay_state.clone())
            }
            _ => None,
        })
        .expect("assistant message should be projected");
    assert_eq!(
        assistant.as_ref().map(|state| state.provider_kind.as_str()),
        Some("openai_responses")
    );
}

async fn drain_invocation(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>) {
    loop {
        let message = next_message(receiver).await;
        if matches!(
            message.message,
            Some(run_inference_server_message::Message::InvocationEnd(_))
        ) {
            return;
        }
    }
}

#[tokio::test]
async fn detailed_logging_records_the_local_loop_trace() {
    let (_directory, store) = fixtures::temp_store().await;
    store.set_detailed_logging(true).await.unwrap();
    let model = store.create_model(&model_input()).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::TextDelta("hi".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(cursor_server::provider::FinishReason::Stop),
    ]);

    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider),
        CursorTraceService::new(store.clone()),
    );
    let handle = registry
        .ensure_local(
            "request-trace",
            Some("conversation-1"),
            Some(&model.model_hash),
        )
        .await
        .unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append("request-trace", run_request(&model.model_hash))
        .await
        .unwrap();
    let _ready = next_message(&mut receiver).await;
    registry
        .append("request-trace", invoke_model(&model.model_hash))
        .await
        .unwrap();
    drain_invocation(&mut receiver).await;
    registry
        .append(
            "request-trace",
            RunInferenceClientMessage {
                message: Some(run_inference_client_message::Message::FinishRun(
                    RunInferenceFinishRun {
                        aborted: false,
                        turn_completed: Some(true),
                    },
                )),
            },
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let trace = loop {
        if let Some(trace) = store.cursor_trace("request-trace").await.unwrap() {
            if trace.finished_at_ms.is_some() {
                break trace;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "trace was not recorded"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(trace.route, "local_byok");
    assert_eq!(trace.model_id.as_deref(), Some(model.model_hash.as_str()));
    assert_eq!(trace.conversation_id.as_deref(), Some("conversation-1"));
    assert!(trace.request_bytes > 0);
    assert!(trace.response_bytes > 0);
}

#[tokio::test]
async fn finish_run_closes_the_inference_stream() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store.create_model(&model_input()).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider),
        CursorTraceService::new(store),
    );
    let handle = registry
        .ensure_local("request-2", None, None)
        .await
        .unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append("request-2", run_request(&model.model_hash))
        .await
        .unwrap();
    let _ready = next_message(&mut receiver).await;

    registry
        .append(
            "request-2",
            RunInferenceClientMessage {
                message: Some(run_inference_client_message::Message::FinishRun(
                    RunInferenceFinishRun {
                        aborted: false,
                        turn_completed: Some(true),
                    },
                )),
            },
        )
        .await
        .unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("timed out waiting for the stream close")
        .expect("expected an end-stream frame");
    let frames = connect::decode_frames(&frame).unwrap();
    assert!(
        frames
            .iter()
            .any(|(flags, _)| flags & connect::END_STREAM_FLAG != 0),
        "finish_run must end the Connect stream"
    );

    // The session removes itself after the stream closes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while registry.local("request-2").await.is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session was not released"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn run_inference_sse_route_body_decodes() {
    let request = ai::BidiRequestId {
        request_id: "request-1".into(),
    };
    let body = connect::encode_message(&request).unwrap();
    let decoded: ai::BidiRequestId = connect::decode_unary(&body).unwrap();
    assert_eq!(decoded.request_id, "request-1");
}
