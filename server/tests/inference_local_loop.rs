//! Verifies the RunInference bridge used by Cursor's local agent loop.
#![allow(dead_code)]

#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;
#[path = "support/gemini.rs"]
mod gemini;

use std::{sync::Arc, time::Duration};

use axum::{http::header, routing::post, Json, Router};
use cursor_server::{
    cursor::{
        inference::{wire::*, InferenceRegistry},
        protocol::{connect, proto::aiserver::v1 as ai},
        services::observability::CursorTraceService,
    },
    model::{ModelConfigInput, ModelType, Usage, OPENAI_CHAT_ENDPOINT, OPENAI_RESPONSES_ENDPOINT},
    network::NetworkClients,
    plugin::{PluginRegistry, PluginRuntime},
    provider::{ModelEvent, ProviderRouter},
};
use prost::Message;
use serde_json::{json, Value};

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

fn selection(model_id: &str, parameters: &[(&str, &str)]) -> InferenceRequestedModel {
    InferenceRequestedModel {
        model_id: model_id.into(),
        parameters: parameters
            .iter()
            .map(|&(id, value)| InferenceModelParameterValue {
                id: id.into(),
                value: value.into(),
            })
            .collect(),
        ..Default::default()
    }
}

fn run_request(model: InferenceRequestedModel) -> RunInferenceClientMessage {
    RunInferenceClientMessage {
        message: Some(run_inference_client_message::Message::RunRequest(
            RunInferenceRunRequest {
                conversation_id: "conversation-1".into(),
                requested_model: Some(model),
                ..Default::default()
            },
        )),
    }
}

fn invoke_model(model_id: &str) -> RunInferenceClientMessage {
    invoke(
        "invocation-1",
        InferenceStreamRequest {
            messages: vec![InferenceCoreMessage {
                role: MESSAGE_ROLE_USER,
                content: Some(inference_core_message::Content::Text("hello".into())),
                ..Default::default()
            }],
            model_id: model_id.into(),
            conversation_id: "conversation-1".into(),
            ..Default::default()
        },
    )
}

fn invoke(id: &str, request: InferenceStreamRequest) -> RunInferenceClientMessage {
    RunInferenceClientMessage {
        message: Some(run_inference_client_message::Message::InvokeModel(
            RunInferenceInvokeModel {
                invocation_id: id.into(),
                request: Some(request),
            },
        )),
    }
}

#[tokio::test]
async fn background_summary_and_main_call_keep_their_own_model_parameters() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store
        .create_model(&ModelConfigInput {
            context_window_tokens: Some(272_000),
            reasoning_effort: Some("max".into()),
            ..model_input()
        })
        .await
        .unwrap();
    let provider = fake_provider::FakeProvider::default();
    let finish = || {
        vec![ModelEvent::Done(
            cursor_server::provider::FinishReason::Stop,
        )]
    };
    let main_call = provider.push_gated(finish());
    provider.push(finish());
    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        CursorTraceService::new(store),
    );
    let handle = registry.ensure_local("run", None, None).await.unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append("run", run_request(selection(&model.model_hash, &[])))
        .await
        .unwrap();
    let Some(run_inference_server_message::Message::RunReady(ready)) =
        next_message(&mut receiver).await.message
    else {
        panic!("expected RunReady");
    };
    let metadata = ready.prompt_model_metadata.unwrap();
    assert_eq!(metadata.agent_token_limit, Some(272_000));
    assert_eq!(metadata.reasoning_effort.as_deref(), Some("max"));

    registry
        .append("run", invoke_model(&model.model_hash))
        .await
        .unwrap();
    registry
        .append(
            "run",
            invoke(
                "summary",
                InferenceStreamRequest {
                    requested_model: Some(selection(
                        &model.model_hash,
                        &[("context", "200k"), ("reasoning", "low")],
                    )),
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();

    let summary = drain_invocation(&mut receiver).await;
    assert_eq!(summary.invocation_id, "summary");
    assert!(summary.error.is_none());
    main_call.notify_one();
    let main = drain_invocation(&mut receiver).await;
    assert_eq!(main.invocation_id, "invocation-1");
    assert!(main.error.is_none());

    let requests = provider.requests();
    assert_eq!(requests[0].model.context_window_tokens, Some(272_000));
    assert_eq!(requests[0].model.reasoning.effort.as_deref(), Some("max"));
    assert_eq!(requests[1].model.context_window_tokens, Some(200_000));
    assert_eq!(requests[1].model.reasoning.effort.as_deref(), Some("low"));
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
    let model = store
        .create_model(&ModelConfigInput {
            context_window_tokens: Some(1_000_000),
            reasoning_effort: Some("max".into()),
            ..model_input()
        })
        .await
        .unwrap();
    let selected = selection(
        &model.model_hash,
        &[("context", "272000"), ("reasoning", "xhigh")],
    );
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
        ModelEvent::Usage(Usage {
            context_input_tokens: Some(246_000),
            ..Default::default()
        }),
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
        .append("request-1", run_request(selected.clone()))
        .await
        .unwrap();

    let ready = next_message(&mut receiver).await;
    let run_inference_server_message::Message::RunReady(ready) = ready.message.unwrap() else {
        panic!("expected run_ready");
    };
    assert_eq!(ready.resolved_model.unwrap(), selected);
    assert!(ready.supports_self_summary);
    let metadata = ready
        .prompt_model_metadata
        .expect("local loop requires prompt model metadata");
    assert_eq!(metadata.vendor, "openai");
    assert_eq!(metadata.prompt_version, "latest");
    assert_eq!(metadata.agent_token_limit, Some(272_000));
    assert_eq!(metadata.reasoning_effort.as_deref(), Some("xhigh"));
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
    let mut usage = None;
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
                    inference_stream_response::Response::Usage(_) => {}
                    inference_stream_response::Response::ExtendedUsage(part) => usage = Some(part),
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
    let usage = usage.expect("Cursor needs extended usage to trigger compaction");
    assert_eq!(usage.max_tokens, 272_000);
    assert_eq!(usage.context_input_tokens, Some(246_000));
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
    assert_eq!(requests[0].model.context_window_tokens, Some(272_000));
    assert_eq!(requests[0].model.reasoning.effort.as_deref(), Some("xhigh"));
}

#[tokio::test]
async fn oversized_history_requests_compaction_before_contacting_the_provider() {
    let (_directory, store) = fixtures::temp_store().await;
    let model = store.create_model(&model_input()).await.unwrap();
    let provider = fake_provider::FakeProvider::default();
    provider.push_error(cursor_server::Error::Provider(
        "context_length_exceeded".into(),
    ));
    provider.push(vec![ModelEvent::Done(
        cursor_server::provider::FinishReason::Stop,
    )]);
    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        CursorTraceService::new(store),
    );
    let handle = registry
        .ensure_local("resumed-run", None, None)
        .await
        .unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append(
            "resumed-run",
            run_request(selection(&model.model_hash, &[("context", "272000")])),
        )
        .await
        .unwrap();
    let _ready = next_message(&mut receiver).await;

    for (id, text, provider_calls) in [
        ("restored-history", "word ".repeat(1_000_000), 0),
        (
            "provider-overflow",
            "input the provider counts differently".into(),
            1,
        ),
    ] {
        registry
            .append(
                "resumed-run",
                invoke(
                    id,
                    InferenceStreamRequest {
                        messages: vec![InferenceCoreMessage {
                            role: MESSAGE_ROLE_USER,
                            content: Some(inference_core_message::Content::Text(text)),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ),
            )
            .await
            .unwrap();
        let Some(run_inference_server_message::Message::InvocationResponse(response)) =
            next_message(&mut receiver).await.message
        else {
            panic!("expected an inference response")
        };
        assert_eq!(response.invocation_id, id);
        let Some(inference_stream_response::Response::Error(error)) =
            response.response.unwrap().response
        else {
            panic!("Cursor needs an input-token-limit error to compact and retry");
        };
        assert!(error.is_input_token_limit_error);
        assert_eq!(drain_invocation(&mut receiver).await.invocation_id, id);
        assert_eq!(provider.requests().len(), provider_calls);
    }
    registry
        .append("resumed-run", invoke_model(&model.model_hash))
        .await
        .unwrap();
    let retry = drain_invocation(&mut receiver).await;
    assert!(retry.error.is_none());
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn openai_http_requests_carry_the_selected_effort() {
    let (_directory, store) = fixtures::temp_store().await;
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let upstream = Router::new().route(
        "/{*path}",
        post(move |Json(body): Json<Value>| {
            let sent = sent.clone();
            async move {
                let response = if body.get("messages").is_some() {
                    "data: [DONE]\n\n"
                } else {
                    "data: {\"type\":\"response.completed\"}\n\n"
                };
                sent.send(body).unwrap();
                ([(header::CONTENT_TYPE, "text/event-stream")], response)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let config = ModelConfigInput {
        base_url,
        reasoning_effort: Some("max".into()),
        context_window_tokens: Some(272_000),
        ..model_input()
    };
    let chat = store.create_model(&config).await.unwrap();
    let responses = store
        .create_model(&ModelConfigInput {
            openai_endpoint: OPENAI_RESPONSES_ENDPOINT.into(),
            ..config
        })
        .await
        .unwrap();
    let plugins = PluginRegistry::managed(
        store.clone(),
        PluginRuntime::managed().unwrap(),
        "test".into(),
    )
    .unwrap();
    let provider = ProviderRouter::new(
        store.clone(),
        plugins,
        NetworkClients::new(store.clone()),
        Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let registry = InferenceRegistry::new(
        store.clone(),
        Arc::new(provider),
        CursorTraceService::new(store),
    );
    let handle = registry.ensure_local("http", None, None).await.unwrap();
    let mut receiver = handle.subscribe();
    registry
        .append(
            "http",
            run_request(selection(&chat.model_hash, &[("reasoning", "xhigh")])),
        )
        .await
        .unwrap();
    let _ready = next_message(&mut receiver).await;

    for (id, requested, pointer, effort) in [
        ("chat", None, "/reasoning_effort", "xhigh"),
        (
            "responses",
            Some(selection(&responses.model_hash, &[("reasoning", "low")])),
            "/reasoning/effort",
            "low",
        ),
        (
            "responses-default",
            Some(selection(&responses.model_hash, &[])),
            "/reasoning/effort",
            "max",
        ),
    ] {
        registry
            .append(
                "http",
                invoke(
                    id,
                    InferenceStreamRequest {
                        requested_model: requested,
                        ..Default::default()
                    },
                ),
            )
            .await
            .unwrap();
        let end = drain_invocation(&mut receiver).await;
        assert!(end.error.is_none(), "{:?}", end.error);
        let body = received.try_recv().unwrap();
        assert_eq!(body.pointer(pointer).and_then(Value::as_str), Some(effort));
    }
    server.abort();
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
        .append(
            "request-replay",
            run_request(selection(&model.model_hash, &[])),
        )
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

#[tokio::test]
async fn gemini_discovery_connectivity_and_local_parallel_tools() {
    let (_directory, store) = fixtures::temp_store().await;
    let parts = vec![
        json!({"functionCall":{"name":"Read","args":{"path":"a"},"id":"upstream-A"},"thoughtSignature":gemini::SIGNATURE}),
        json!({"functionCall":{"name":"Read","args":{"path":"b"},"id":"upstream-B"}}),
    ];
    let mut channel = gemini::channel(&store, parts.clone()).await;
    let mut discovery = cursor_server::control::ModelDiscoveryInput {
        model_type: ModelType::Gemini,
        base_url: channel.model.base_url.clone(),
        api_key: "invalid".into(),
        custom_headers_enabled: false,
        custom_headers: json!({}),
    };
    assert!(channel
        .control
        .discover_models(&discovery)
        .await
        .unwrap_err()
        .to_string()
        .contains("401"));
    discovery.api_key = channel.model.api_key.clone();
    assert_eq!(
        channel
            .control
            .discover_models(&discovery)
            .await
            .unwrap()
            .models,
        [gemini::MODEL, "z"]
    );
    let tested = channel
        .control
        .test_model(&channel.model.model_hash, "gemini-test")
        .await
        .unwrap();
    assert_eq!(tested.output, "all done");
    assert_eq!(tested.output_tokens, 4);
    assert!(channel
        .requests
        .try_recv()
        .unwrap()
        .get("generationConfig")
        .is_none());
    let registry = InferenceRegistry::new(
        store.clone(),
        channel.provider.clone(),
        CursorTraceService::new(store),
    );
    let id = "gemini-inference";
    let handle = registry.ensure_local(id, None, None).await.unwrap();
    let mut output = handle.subscribe();
    registry
        .append(id, run_request(selection(&channel.model.model_hash, &[])))
        .await
        .unwrap();
    next_message(&mut output).await;
    registry
        .append(
            id,
            invoke(
                "first",
                InferenceStreamRequest {
                    tools: vec![InferenceAgentTool {
                        name: "Read".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    assert!(drain_invocation(&mut output).await.error.is_none());
    let _first = channel.requests.try_recv().unwrap();
    let calls = ["upstream-B", "upstream-A"]
        .into_iter()
        .map(|id| InferenceToolCall {
            tool_call_id: id.into(),
            tool_name: "Read".into(),
            ..Default::default()
        })
        .collect();
    let results = ["upstream-B", "upstream-A"]
        .into_iter()
        .map(|id| InferenceToolResultPart {
            tool_call_id: id.into(),
            tool_name: "Read".into(),
            result: Some(prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(id.into())),
            }),
            ..Default::default()
        })
        .collect();
    registry
        .append(
            id,
            invoke(
                "second",
                InferenceStreamRequest {
                    messages: vec![
                        InferenceCoreMessage {
                            role: MESSAGE_ROLE_ASSISTANT,
                            tool_calls: calls,
                            ..Default::default()
                        },
                        InferenceCoreMessage {
                            role: MESSAGE_ROLE_TOOL,
                            content: Some(inference_core_message::Content::ToolContent(
                                InferenceToolResultContent { parts: results },
                            )),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let end = drain_invocation(&mut output).await;
    assert!(end.error.is_none(), "{:?}", end.error);
    let second = channel.requests.try_recv().unwrap();
    assert_eq!(second["contents"][0]["parts"][1], parts[0]);
    assert_eq!(second["contents"][0]["parts"][2], parts[1]);
    let responses = second["contents"][1]["parts"].as_array().unwrap();
    for (part, id) in responses.iter().zip(["upstream-A", "upstream-B"]) {
        assert_eq!(part["functionResponse"]["id"], id);
        assert_eq!(part["functionResponse"]["response"]["output"], id);
    }
}

async fn drain_invocation(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>,
) -> RunInferenceInvocationEnd {
    loop {
        let message = next_message(receiver).await;
        if let Some(run_inference_server_message::Message::InvocationEnd(end)) = message.message {
            return end;
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
        .append(
            "request-trace",
            run_request(selection(&model.model_hash, &[])),
        )
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
        .append("request-2", run_request(selection(&model.model_hash, &[])))
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
