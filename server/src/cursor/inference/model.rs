//! Resolves the model and picker parameters for each local-loop invocation.
use crate::{
    cursor::compile::apply_model_parameters,
    model::{ModelConfig, ModelSpec, ModelType},
    store::Store,
    Error, Result,
};

use super::wire::{InferenceRequestedModel, InferenceStreamRequest};

pub(super) async fn configured_model(store: &Store, id: &str) -> Result<Option<ModelConfig>> {
    if id.starts_with(crate::plugin::ADAPTER_ID_PREFIX) {
        return Ok(None);
    }
    store
        .model(id)
        .await?
        .map(Some)
        .ok_or_else(|| Error::Protocol(format!("unknown model: {id}")))
}

#[derive(Clone)]
pub(super) struct InferenceModel {
    pub spec: ModelSpec,
    pub is_anthropic: bool,
}

impl InferenceModel {
    pub fn new(id: &str, config: Option<&ModelConfig>) -> Self {
        let mut spec = ModelSpec::new(id);
        if let Some(config) = config {
            config.configure(&mut spec);
        }
        Self {
            spec,
            is_anthropic: config.is_some_and(|config| config.model_type == ModelType::Anthropic),
        }
    }

    pub fn select(&mut self, requested: &InferenceRequestedModel) -> Result<()> {
        apply_model_parameters(&mut self.spec, requested.max_mode, &requested.parameters)
    }

    pub async fn for_invocation(
        &self,
        store: &Store,
        request: &InferenceStreamRequest,
    ) -> Result<Self> {
        let requested = request.requested_model.as_ref();
        let id = requested
            .map(|model| model.model_id.trim())
            .filter(|id| !id.is_empty())
            .unwrap_or(request.model_id.trim());
        let mut model = if id.is_empty() || id == self.spec.model_id {
            self.clone()
        } else {
            Self::new(id, configured_model(store, id).await?.as_ref())
        };
        if let Some(requested) = requested {
            model.select(requested)?;
        }
        Ok(model)
    }
}
