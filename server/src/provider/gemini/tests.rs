use super::*;
use crate::model::{
    gemini_models_url, is_sensitive_header, resolve_request_url, ContentPart, ModelRequest,
    ModelSpec, ModelType, ProjectedContent, ProjectedMessage, PromptSpec, Role,
};

#[test]
fn native_urls_preserve_channel_prefix_and_validate_full_endpoint() {
    let expected = "https://example.org/proxy/v1beta/models/test:streamGenerateContent?alt=sse";
    for suffix in ["", "/v1beta", "/v1beta/models/"] {
        let base = format!("https://example.org/proxy{suffix}");
        assert_eq!(
            resolve_request_url(ModelType::Gemini, &base, "models/test", "", false).unwrap(),
            expected
        );
        assert_eq!(
            gemini_models_url(&base).unwrap().path(),
            "/proxy/v1beta/models"
        );
    }
    assert_eq!(
        resolve_request_url(ModelType::Gemini, expected, "test", "", true).unwrap(),
        expected
    );
    assert!(resolve_request_url(
        ModelType::Gemini,
        "https://example.org/v1beta/models",
        "test",
        "",
        true
    )
    .is_err());
    assert!(resolve_request_url(ModelType::Gemini, expected, "other", "", true).is_err());
    assert_eq!(
        gemini_models_url(expected).unwrap().as_str(),
        "https://example.org/proxy/v1beta/models"
    );
    assert!(is_sensitive_header("X-Goog-Api-Key"));
}

#[test]
fn text_images_and_explicit_output_limit_are_encoded() {
    let mut request = ModelRequest {
        prompt: PromptSpec {
            instructions: "Reply briefly".into(),
            tools: vec![],
        },
        model: ModelSpec::new("test"),
        history: vec![ProjectedMessage {
            message_id: "one".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![
                ContentPart::Text {
                    text: "describe".into(),
                },
                ContentPart::Image {
                    mime_type: "image/png".into(),
                    data: vec![1, 2, 3],
                },
            ]),
        }],
    };
    let default = request::body(&request, None).unwrap();
    assert!(default.get("generationConfig").is_none());
    assert_eq!(
        default["systemInstruction"]["parts"][0]["text"],
        "Reply briefly"
    );
    assert_eq!(
        default["contents"][0]["parts"][1]["inlineData"],
        json!({"mimeType":"image/png","data":"AQID"})
    );
    request.model.max_output_tokens = Some(123);
    assert_eq!(
        request::body(&request, Some(42)).unwrap()["generationConfig"]["maxOutputTokens"],
        123
    );
}

#[test]
fn truncated_filtered_and_unfinished_streams_never_emit_tools() {
    let candidate = json!({"content":{"parts":[{"functionCall":{"name":"Read","args":{}}}]}});
    for reason in [
        None,
        Some("SAFETY"),
        Some("MALFORMED_FUNCTION_CALL"),
        Some("MAX_TOKENS"),
    ] {
        let mut state = Response::default();
        let mut c = candidate.clone();
        if let Some(reason) = reason {
            c["finishReason"] = json!(reason);
        }
        assert!(state.chunk(json!({"candidates":[c]})).unwrap().is_empty());
        let finish = state.finish("test");
        if reason == Some("MAX_TOKENS") {
            assert_eq!(
                finish.unwrap(),
                vec![ModelEvent::Done(FinishReason::Length)]
            );
        } else {
            assert!(finish.is_err());
        }
    }
    assert!(Response::default()
        .chunk(json!({"promptFeedback":{"blockReason":"SAFETY"}}))
        .is_err());
}
