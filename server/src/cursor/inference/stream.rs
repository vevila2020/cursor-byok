//! Streams local-loop inference output to Cursor and relays official runs.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{
    cursor::inference::{session::is_end_stream, InferenceHandle, InferenceRegistry},
    Result,
};

pub async fn local(handle: &InferenceHandle) -> Result<Response<Body>> {
    let receiver = handle.subscribe();
    let body_stream = local_body_stream(receiver, handle.clone());
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("connect-protocol-version", HeaderValue::from_static("1"));
    Ok(response)
}

fn local_body_stream(
    mut receiver: mpsc::UnboundedReceiver<Bytes>,
    handle: InferenceHandle,
) -> impl tokio_stream::Stream<Item = std::result::Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut guard = LocalGuard::new(handle);
        while let Some(chunk) = receiver.recv().await {
            let terminal = is_end_stream(&chunk);
            yield Ok::<Bytes, Infallible>(chunk);
            if terminal {
                guard.complete();
                return;
            }
        }
        guard.complete();
    }
}

struct LocalGuard {
    handle: InferenceHandle,
    completed: bool,
}

impl LocalGuard {
    fn new(handle: InferenceHandle) -> Self {
        Self {
            handle,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.disconnect().await;
        });
    }
}

pub async fn upstream(
    registry: InferenceRegistry,
    request_id: String,
    generation: u64,
    response: Response<Body>,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let _guard = UpstreamGuard {
            registry,
            request_id,
            generation,
        };
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => yield Ok::<Bytes, axum::Error>(chunk),
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

struct UpstreamGuard {
    registry: InferenceRegistry,
    request_id: String,
    generation: u64,
}

impl Drop for UpstreamGuard {
    fn drop(&mut self) {
        self.registry
            .finish_upstream(self.request_id.clone(), self.generation);
    }
}
