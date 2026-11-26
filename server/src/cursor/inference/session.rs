//! Owns request-ID-scoped local-loop inference sessions and their provider calls.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, Mutex, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::{
        inference::{
            convert::{self, SessionInfo},
            model::{configured_model, InferenceModel},
            wire::*,
        },
        protocol::connect::{self, ConnectCode, ConnectStreamError, END_STREAM_FLAG},
        services::observability::{CursorTraceRecorder, CursorTraceService},
        transport::OutputHub,
    },
    model::{estimate_context_tokens, ModelInvocation, Usage},
    provider::{ModelEvent, Provider, ProviderStream},
    store::Store,
    Error, Result,
};

const COMMAND_BUFFER: usize = 128;
const EVENT_BUFFER: usize = 256;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct InferenceRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    store: Store,
    provider: Arc<dyn Provider>,
    traces: CursorTraceService,
    thinking: Arc<convert::ThinkingTextCache>,
    sessions: Mutex<HashMap<String, LocalSession>>,
    upstream: Mutex<HashMap<String, u64>>,
    opened: Mutex<HashSet<String>>,
    changed: Notify,
    next_generation: AtomicU64,
}

#[derive(Clone)]
struct LocalSession {
    generation: u64,
    handle: InferenceHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceRoute {
    Local,
    Upstream(u64),
}

#[derive(Clone)]
pub struct InferenceHandle {
    request_id: String,
    commands: mpsc::Sender<Command>,
    output: Arc<OutputHub>,
    trace: CursorTraceRecorder,
}

impl InferenceHandle {
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<bytes::Bytes> {
        self.output.subscribe()
    }

    pub async fn disconnect(&self) {
        let _ = self.commands.send(Command::Disconnect).await;
    }

    fn emit(&self, message: &RunInferenceServerMessage) {
        match connect::encode_message(message) {
            Ok(frame) => {
                self.trace.response_chunk("byok_server", frame.clone());
                self.output.emit(frame);
            }
            Err(error) => {
                tracing::error!(
                    request_id = self.request_id,
                    %error,
                    "failed to encode RunInference server message"
                );
            }
        }
    }

    fn trace(&self) -> &CursorTraceRecorder {
        &self.trace
    }

    fn close_with_error(&self, message: String) {
        self.trace.finish(Some(&message));
        if let Ok(frame) = connect::encode_error_end_stream(&ConnectStreamError {
            code: ConnectCode::InvalidArgument,
            message,
            details: Vec::new(),
        }) {
            self.output.emit(frame);
        }
        self.output.close();
    }

    fn close(&self) {
        self.trace.finish(None);
        self.output.emit(connect::encode_end_stream());
        self.output.close();
    }
}

enum Command {
    Append(Box<RunInferenceClientMessage>),
    CancelActive,
    Disconnect,
}

enum SessionEvent {
    Frame(Box<RunInferenceServerMessage>),
    Replay {
        call_ids: Vec<String>,
        state: crate::model::ProviderReplayState,
    },
    End(Option<(i32, String)>),
}

struct InvocationEvents {
    index: u64,
    invocation_id: String,
    sender: mpsc::Sender<(u64, SessionEvent)>,
}

impl InvocationEvents {
    async fn send(&self, event: SessionEvent) {
        let _ = self.sender.send((self.index, event)).await;
    }

    async fn respond(&self, part: inference_stream_response::Response) {
        let message = convert::invocation_response(&self.invocation_id, part);
        self.send(SessionEvent::Frame(Box::new(message))).await;
    }
}

impl InferenceRegistry {
    pub fn new(store: Store, provider: Arc<dyn Provider>, traces: CursorTraceService) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                store,
                provider,
                traces,
                thinking: Arc::new(convert::ThinkingTextCache::default()),
                sessions: Mutex::new(HashMap::new()),
                upstream: Mutex::new(HashMap::new()),
                opened: Mutex::new(HashSet::new()),
                changed: Notify::new(),
                next_generation: AtomicU64::new(1),
            }),
        }
    }

    pub async fn open_stream(&self, request_id: &str) {
        let mut opened = self.inner.opened.lock().await;
        if opened.insert(request_id.into()) {
            drop(opened);
            self.inner.changed.notify_waiters();
        }
    }

    pub async fn has_open(&self, request_id: &str) -> bool {
        if self.inner.opened.lock().await.contains(request_id) {
            return true;
        }
        if self.inner.sessions.lock().await.contains_key(request_id) {
            return true;
        }
        self.inner.upstream.lock().await.contains_key(request_id)
    }

    pub async fn wait_route(&self, request_id: &str) -> InferenceRoute {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.inner.sessions.lock().await.contains_key(request_id) {
                return InferenceRoute::Local;
            }
            if let Some(generation) = self.inner.upstream.lock().await.get(request_id).copied() {
                return InferenceRoute::Upstream(generation);
            }
            changed.await;
        }
    }

    pub async fn local(&self, request_id: &str) -> Option<InferenceHandle> {
        self.inner
            .sessions
            .lock()
            .await
            .get(request_id)
            .map(|session| session.handle.clone())
    }

    pub async fn ensure_local(
        &self,
        request_id: &str,
        conversation_id: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<InferenceHandle> {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(session) = sessions.get(request_id) {
            return Ok(session.handle.clone());
        }
        let (commands, receiver) = mpsc::channel(COMMAND_BUFFER);
        let handle = InferenceHandle {
            request_id: request_id.into(),
            commands,
            output: Arc::new(OutputHub::default()),
            trace: self.inner.traces.recorder(request_id),
        };
        handle
            .trace()
            .begin(conversation_id, "local_byok", model_id);
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        sessions.insert(
            request_id.into(),
            LocalSession {
                generation,
                handle: handle.clone(),
            },
        );
        drop(sessions);
        self.open_stream(request_id).await;
        self.inner.changed.notify_waiters();

        let registry = Arc::downgrade(&self.inner);
        let request_id_owned = request_id.to_string();
        tokio::spawn(run_session(
            registry,
            request_id_owned,
            generation,
            handle.clone(),
            self.inner.store.clone(),
            self.inner.provider.clone(),
            self.inner.thinking.clone(),
            receiver,
        ));
        Ok(handle)
    }

    pub async fn append(&self, request_id: &str, message: RunInferenceClientMessage) -> Result<()> {
        let Some(handle) = self.local(request_id).await else {
            return Err(Error::RunNotFound(request_id.into()));
        };
        let payload = prost::Message::encode_to_vec(&message);
        handle.trace().request(
            "run_inference_append",
            bytes::Bytes::from(payload),
            serde_json::json!({
                "request_id": request_id,
                "message_type": super::message_kind(&message),
            }),
        );
        handle
            .commands
            .send(Command::Append(Box::new(message)))
            .await
            .map_err(|_| Error::RunNotFound(request_id.into()))
    }

    pub async fn cancel(&self, request_id: &str) {
        if let Some(handle) = self.local(request_id).await {
            let _ = handle.commands.send(Command::CancelActive).await;
        }
    }

    pub async fn mark_upstream(&self, request_id: &str) -> u64 {
        let mut upstream = self.inner.upstream.lock().await;
        let generation = upstream.get(request_id).copied().unwrap_or_default() + 1;
        upstream.insert(request_id.into(), generation);
        drop(upstream);
        self.inner.changed.notify_waiters();
        generation
    }

    pub async fn upstream(&self, request_id: &str) -> Option<u64> {
        self.inner.upstream.lock().await.get(request_id).copied()
    }

    pub fn finish_upstream(&self, request_id: String, generation: u64) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut upstream = registry.inner.upstream.lock().await;
            if upstream.get(&request_id) == Some(&generation) {
                upstream.remove(&request_id);
            }
            drop(upstream);
            registry.inner.opened.lock().await.remove(&request_id);
        });
    }

    pub async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.inner.sessions.lock().await);
        self.inner.upstream.lock().await.clear();
        for session in sessions.into_values() {
            let _ = session.handle.commands.send(Command::Disconnect).await;
        }
    }
}

struct PreparedSession {
    conversation_id: String,
    model: InferenceModel,
}

struct ActiveInvocation {
    invocation_id: String,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    registry: Weak<RegistryInner>,
    request_id: String,
    generation: u64,
    handle: InferenceHandle,
    store: Store,
    provider: Arc<dyn Provider>,
    thinking: Arc<convert::ThinkingTextCache>,
    mut commands: mpsc::Receiver<Command>,
) {
    let (events_tx, mut events) = mpsc::channel(EVENT_BUFFER);
    let mut session = None;
    let mut call_index = 0;
    let mut replay = convert::ReplayCache::default();
    // A background summary and the main model can be streaming at the same time.
    let mut active = BTreeMap::<u64, ActiveInvocation>::new();
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
        HEARTBEAT_INTERVAL,
    );

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(Command::Disconnect) => {
                    break;
                }
                Some(Command::CancelActive) => {
                    for invocation in std::mem::take(&mut active).into_values() {
                        invocation.stop(&handle, true);
                    }
                }
                Some(Command::Append(message)) => match *message {
                    RunInferenceClientMessage { message: Some(run_inference_client_message::Message::RunRequest(request)) } => {
                        match prepare_session(&handle, &store, &request_id, &request).await {
                            Ok(prepared) => session = Some(prepared),
                            Err(error) => {
                                tracing::warn!(request_id, %error, "RunInference session rejected its model");
                                handle.close_with_error(error.to_string());
                                break;
                            }
                        }
                    }
                    RunInferenceClientMessage { message: Some(run_inference_client_message::Message::InvokeModel(invocation)) } => {
                        if active.values().any(|call| call.invocation_id == invocation.invocation_id) {
                            continue;
                        }
                        let Some(session) = session.as_ref() else {
                            handle.emit(&convert::invocation_end(
                                &invocation.invocation_id,
                                Some((13, "RunInference invoke_model arrived before run_request".into())),
                            ));
                            continue;
                        };
                        let Some(request) = invocation.request.as_ref() else {
                            handle.emit(&convert::invocation_end(
                                &invocation.invocation_id,
                                Some((13, "RunInference invoke_model has no request".into())),
                            ));
                            continue;
                        };
                        let index = call_index;
                        call_index += 1;
                        let model = session.model.for_invocation(&store, request).await;
                        let call = model.and_then(|model| {
                            let info = SessionInfo {
                                request_id: &request_id,
                                conversation_id: &session.conversation_id,
                                model,
                            };
                            convert::invocation_from_request(info, &invocation.invocation_id, request, index, &replay, &thinking)
                        });
                        match call {
                            Ok(call) => {
                                let output = InvocationEvents {
                                    index,
                                    invocation_id: invocation.invocation_id,
                                    sender: events_tx.clone(),
                                };
                                if let Some(call) = ActiveInvocation::start(call, output, &handle, provider.as_ref(), thinking.clone()) {
                                    active.insert(index, call);
                                }
                            }
                            Err(error) => {
                                handle.emit(&convert::invocation_end(
                                    &invocation.invocation_id,
                                    Some((13, error.to_string())),
                                ));
                            }
                        }
                    }
                    RunInferenceClientMessage { message: Some(run_inference_client_message::Message::CancelInvocation(cancel)) } => {
                        active.retain(|_, call| {
                            if call.invocation_id == cancel.invocation_id {
                                call.stop(&handle, true);
                                return false;
                            }
                            true
                        });
                    }
                    RunInferenceClientMessage { message: Some(run_inference_client_message::Message::FinishRun(_)) } => {
                        break;
                    }
                    RunInferenceClientMessage { message: Some(run_inference_client_message::Message::ClientHeartbeat(_)) } => {
                        if session.is_some() {
                            handle.emit(&convert::heartbeat());
                        }
                    }
                    RunInferenceClientMessage { message: Some(
                        run_inference_client_message::Message::DeltaBaseSeed(_)
                        | run_inference_client_message::Message::ResolveSubagentModels(_)
                        | run_inference_client_message::Message::RunRequestLog(_)
                        | run_inference_client_message::Message::AgentEvent(_)
                        | run_inference_client_message::Message::ClientFirstCallTimings(_),
                    ) }
                    | RunInferenceClientMessage { message: None } => {}
                },
            },
            Some((index, event)) = events.recv() => {
                let Some(call) = active.get(&index) else { continue; };
                match event {
                    SessionEvent::Frame(message) => handle.emit(&message),
                    SessionEvent::Replay { call_ids, state } => replay.insert(call_ids, state),
                    SessionEvent::End(error) => {
                        handle.emit(&convert::invocation_end(&call.invocation_id, error));
                        active.remove(&index);
                    }
                }
            },
            _ = heartbeat.tick() => {
                if session.is_some() {
                    handle.emit(&convert::heartbeat());
                }
            }
        }
    }

    for invocation in active.into_values() {
        invocation.stop(&handle, false);
    }
    handle.close();
    if let Some(inner) = registry.upgrade() {
        let mut sessions = inner.sessions.lock().await;
        if sessions
            .get(&request_id)
            .is_some_and(|session| session.generation == generation)
        {
            sessions.remove(&request_id);
        }
        drop(sessions);
        inner.opened.lock().await.remove(&request_id);
    }
}

async fn prepare_session(
    handle: &InferenceHandle,
    store: &Store,
    request_id: &str,
    request: &RunInferenceRunRequest,
) -> Result<PreparedSession> {
    let requested = request
        .requested_model
        .as_ref()
        .filter(|model| !model.model_id.trim().is_empty())
        .ok_or_else(|| Error::Protocol("RunInference run_request has no requested model".into()))?;
    let model_id = requested.model_id.trim();
    let config = configured_model(store, model_id).await?;
    let mut model = InferenceModel::new(model_id, config.as_ref());
    model.select(requested)?;
    let conversation_id = if request.conversation_id.is_empty() {
        request_id.into()
    } else {
        request.conversation_id.clone()
    };
    let prompt_model_metadata = convert::prompt_metadata(&model.spec, config.as_ref());
    handle.trace().response_started(200);
    handle.emit(&convert::run_ready(
        requested.clone(),
        model.spec.display_name.as_deref(),
        prompt_model_metadata,
    ));
    tracing::info!(
        request_id,
        model_id,
        conversation_id,
        "RunInference session ready"
    );
    Ok(PreparedSession {
        conversation_id,
        model,
    })
}

async fn run_invocation(
    events: InvocationEvents,
    model_id: String,
    context_window: Option<u64>,
    estimated_input: u64,
    mut stream: ProviderStream,
    thinking: Arc<convert::ThinkingTextCache>,
) {
    let mut calls = BTreeMap::<usize, PendingToolCall>::new();
    let mut tool_call_ids = Vec::<String>::new();
    let mut output = convert::InvocationOutput {
        text: String::new(),
        tool_calls: Vec::new(),
        reasoning_parts: Vec::new(),
    };
    let mut usage_reported = false;
    let error = loop {
        let Some(event) = stream.next().await else {
            break Some(("provider stream ended without completion".to_owned(), 13));
        };
        match event {
            Ok(ModelEvent::TextDelta(text)) => {
                output.text.push_str(&text);
                events.respond(convert::text_part(text, false)).await;
            }
            Ok(ModelEvent::ThinkingDelta(text)) => {
                events.respond(convert::thinking_part(text, false)).await;
            }
            Ok(ModelEvent::ThinkingEnd) => {
                // Do not emit an empty final thinking part: Cursor would start
                // a new reasoning segment for it, leaving the signature attached
                // to empty text instead of the streamed thinking block.
            }
            Ok(ModelEvent::ToolCallStart {
                index,
                call_id,
                name,
            }) => {
                events
                    .respond(convert::tool_call_start(&call_id, &name, index))
                    .await;
                calls.insert(
                    index,
                    PendingToolCall {
                        call_id,
                        name,
                        arguments: String::new(),
                    },
                );
            }
            Ok(ModelEvent::ToolCallArgumentsDelta { index, delta }) => {
                if let Some(call) = calls.get_mut(&index) {
                    call.arguments.push_str(&delta);
                    events
                        .respond(convert::tool_call_delta(&call.call_id, &delta, index))
                        .await;
                }
            }
            Ok(ModelEvent::ToolCallEnd { index }) => {
                if let Some(call) = calls.remove(&index) {
                    let arguments = if call.arguments.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        call.arguments
                    };
                    tool_call_ids.push(call.call_id.clone());
                    output.tool_calls.push(convert::tool_call_output(
                        &call.call_id,
                        &call.name,
                        &arguments,
                    ));
                    events
                        .respond(convert::tool_call_part(
                            &call.call_id,
                            &call.name,
                            &arguments,
                            index,
                        ))
                        .await;
                }
            }
            Ok(ModelEvent::Usage(mut usage)) => {
                if usage.context_input_tokens.is_none() && usage.total_tokens.is_none() {
                    usage.context_input_tokens = usage.input_tokens.or(Some(estimated_input));
                }
                for part in convert::usage_parts(&usage, context_window) {
                    events.respond(part).await;
                }
                usage_reported = true;
            }
            Ok(ModelEvent::Done(_)) => break None,
            Ok(ModelEvent::ProviderReplayState(state)) => {
                if state.provider_kind == "anthropic" {
                    if let Some(blocks) =
                        state.value.get("blocks").and_then(|value| value.as_array())
                    {
                        for block in blocks {
                            if block.get("type").and_then(|value| value.as_str())
                                == Some("thinking")
                            {
                                if let (Some(signature), Some(text)) = (
                                    block.get("signature").and_then(|value| value.as_str()),
                                    block.get("thinking").and_then(|value| value.as_str()),
                                ) {
                                    thinking.insert(signature, text);
                                }
                            }
                        }
                    }
                }
                if state.provider_kind == "openai_responses" {
                    events
                        .send(SessionEvent::Replay {
                            call_ids: tool_call_ids.clone(),
                            state: state.clone(),
                        })
                        .await;
                }
                output
                    .reasoning_parts
                    .extend(convert::reasoning_parts_from_state(&state));
                for part in convert::replay_parts(&state) {
                    events.respond(part).await;
                }
            }
            Ok(
                ModelEvent::TextEnd
                | ModelEvent::Start { .. }
                | ModelEvent::TextStart
                | ModelEvent::ThinkingStart,
            ) => {}
            Err(error) => break Some((error.to_string(), error_code(&error))),
        }
    };
    if let Some((message, code)) = error {
        if crate::run::is_context_overflow(&message) {
            events.respond(convert::input_token_limit(message)).await;
            events.send(SessionEvent::End(None)).await;
        } else {
            events.send(SessionEvent::End(Some((code, message)))).await;
        }
        return;
    }
    if !usage_reported && context_window.is_some() {
        let usage = Usage {
            context_input_tokens: Some(estimated_input),
            ..Default::default()
        };
        for part in convert::usage_parts(&usage, context_window) {
            events.respond(part).await;
        }
    }
    if let Some(part) = convert::response_info(&events.invocation_id, &model_id, &output) {
        events.respond(part).await;
    }
    // The local loop completes the model step on the final text part.
    events.respond(convert::text_part("", true)).await;
    events.send(SessionEvent::End(None)).await;
}

struct PendingToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl ActiveInvocation {
    fn start(
        invocation: ModelInvocation,
        output: InvocationEvents,
        handle: &InferenceHandle,
        provider: &dyn Provider,
        thinking: Arc<convert::ThinkingTextCache>,
    ) -> Option<Self> {
        let request = &invocation.request;
        let model_id = request.model.model_id.clone();
        let window = request.model.context_window_tokens;
        let estimate = estimate_context_tokens(&request.prompt, &request.history);
        if let Some(limit) = window.filter(|limit| estimate > *limit) {
            let part = convert::input_token_limit(format!(
                "Estimated input of {estimate} tokens exceeds the selected context window of {limit} tokens"
            ));
            handle.emit(&convert::invocation_response(&output.invocation_id, part));
            handle.emit(&convert::invocation_end(&output.invocation_id, None));
            return None;
        }
        let invocation_id = output.invocation_id.clone();
        let cancellation = CancellationToken::new();
        let stream = provider.stream(invocation, cancellation.clone());
        let task = tokio::spawn(run_invocation(
            output, model_id, window, estimate, stream, thinking,
        ));
        Some(Self {
            invocation_id,
            cancellation,
            task,
        })
    }

    fn stop(&self, handle: &InferenceHandle, notify: bool) {
        self.cancellation.cancel();
        self.task.abort();
        if notify {
            handle.emit(&convert::invocation_end(
                &self.invocation_id,
                Some((1, "Invocation canceled".into())),
            ));
        }
    }
}

fn error_code(error: &Error) -> i32 {
    match error {
        Error::Cancelled => 1,
        Error::Provider(_) | Error::Http(_) => 14,
        _ => 13,
    }
}

pub fn is_end_stream(frame: &bytes::Bytes) -> bool {
    frame
        .first()
        .is_some_and(|flags| flags & END_STREAM_FLAG != 0)
}
