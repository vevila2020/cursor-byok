//! Local Gemini v1beta channel used by both Cursor execution paths.
#![allow(dead_code)]
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use cursor_server::{
    control::ControlService,
    model::{ModelConfig, ModelConfigInput, ModelType},
    network::NetworkClients,
    plugin::{PluginRegistry, PluginRuntime},
    provider::ProviderRouter,
    store::Store,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub const MODEL: &str = "channel-alias";
pub const SIGNATURE: &str = "opaque-test-signature";
#[derive(Clone)]
struct Channel {
    bodies: mpsc::UnboundedSender<Value>,
    calls: Vec<Value>,
}
pub struct Fixture {
    pub model: ModelConfig,
    pub provider: Arc<ProviderRouter>,
    pub control: ControlService,
    pub requests: mpsc::UnboundedReceiver<Value>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub async fn channel(store: &Store, calls: Vec<Value>) -> Fixture {
    let (bodies, requests) = mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/proxy/v1beta/models",
        listener.local_addr().unwrap()
    );
    let routes = Router::new()
        .route("/proxy/v1beta/models", get(list))
        .route(
            "/proxy/v1beta/models/channel-alias:streamGenerateContent",
            post(generate),
        )
        .with_state(Channel { bodies, calls });
    let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    let config:ModelConfigInput=serde_json::from_value(json!({"display_name":"Native","type":"gemini","base_url":url,"model_id":"models/channel-alias","api_key":"channel-secret","tooltip_data":"Gemini"})).unwrap();
    let model = store.create_model(&config).await.unwrap();
    assert_eq!(model.model_type, ModelType::Gemini);
    assert_eq!(model.model_id, MODEL);
    let runtime = PluginRuntime::managed().unwrap();
    let plugins = PluginRegistry::managed(store.clone(), runtime.clone(), "test".into()).unwrap();
    let clients = NetworkClients::new(store.clone());
    let provider = Arc::new(ProviderRouter::new(
        store.clone(),
        plugins.clone(),
        clients.clone(),
        Duration::from_secs(8),
        Duration::from_secs(8),
    ));
    let control =
        ControlService::new(store.clone(), provider.clone(), runtime, plugins, clients).unwrap();
    Fixture {
        model,
        provider,
        control,
        requests,
        server,
    }
}
fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("x-goog-api-key")
        .is_some_and(|h| h == "channel-secret")
}
async fn list(
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let page = if params.get("pageToken").is_some_and(|v| v == "page/2+") {
        json!({"models":[{"name":"models/z","supportedGenerationMethods":["generateContent"]}]})
    } else {
        json!({"models":[{"name":"models/channel-alias","supportedGenerationMethods":["generateContent"]},{"name":"models/embed","supportedGenerationMethods":["embedContent"]}],"nextPageToken":"page/2+"})
    };
    Json(page).into_response()
}
async fn generate(
    State(channel): State<Channel>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if params.get("alt").map(String::as_str) != Some("sse") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let needs_tools = !channel.calls.is_empty()
        && body.get("tools").is_some()
        && !body.to_string().contains("functionResponse");
    channel.bodies.send(body).unwrap();
    let (first, second) = if needs_tools {
        (json!({"thought":true,"text":"thinking"}), channel.calls)
    } else {
        (json!({"text":"all "}), vec![json!({"text":"done"})])
    };
    let chunks = [
        json!({"candidates":[{"content":{"parts":[first]}}],"usageMetadata":{"candidatesTokenCount":1}}),
        json!({"candidates":[{"content":{"parts":second},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":4,"totalTokenCount":16}}),
    ];
    let stream = chunks
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        stream,
    )
        .into_response()
}
