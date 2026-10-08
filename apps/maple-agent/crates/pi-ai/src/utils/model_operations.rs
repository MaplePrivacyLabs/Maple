//! Selected chat model-type operations from `utils/model-operations.ts`.

use crate::types::{Model, ModelType};

pub fn get_model_type(model: &Model) -> ModelType {
    model.r#type.unwrap_or(ModelType::Chat)
}

pub fn is_model_type(model: &Model, model_type: ModelType) -> bool {
    get_model_type(model) == model_type
}
