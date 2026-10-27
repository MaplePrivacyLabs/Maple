//! Allowlisted observability only. Never format API/DB errors or caller-owned
//! strings: their nested values can contain plaintext scheduled for encryption.

use crate::{db::DBError, models::response_compaction::CompactionError, ApiError};

pub(in crate::web::responses) fn model_id(model: &str) -> &'static str {
    crate::model_config::resolve_public_model_id(model).unwrap_or("uncataloged")
}

pub(in crate::web::responses) fn api_error_category(error: &ApiError) -> &'static str {
    match error {
        ApiError::InternalServerError => "internal",
        ApiError::ServiceUnavailable => "unavailable",
        ApiError::InferenceCapacity { .. } => "capacity",
        ApiError::InferenceProvider(_) => "provider",
        ApiError::MessageExceedsContextLimit => "context_limit",
        ApiError::UsageLimitReached => "usage_limit",
        ApiError::FreeTokenLimitExceeded => "free_token_limit",
        ApiError::ModelNotAvailableOnPlan => "model_plan_denied",
        ApiError::ModelNotAvailable => "model_unavailable",
        ApiError::Unauthorized => "unauthorized",
        ApiError::EncryptionError => "encryption",
        ApiError::BadRequest => "bad_request",
        ApiError::PayloadTooLarge => "payload_too_large",
        ApiError::TooManyRequests => "rate_limit",
        _ => "other",
    }
}

pub(in crate::web::responses) fn db_error_category(error: &DBError) -> &'static str {
    match error {
        DBError::ConnectionError => "database_connection",
        DBError::QueryError(_) => "database_query",
        DBError::EncryptionError(_) => "encryption",
        DBError::CompactionError(error) => match error {
            CompactionError::Database(_) => "database_query",
            CompactionError::ConversationNotFound => "conversation_not_found",
            CompactionError::InvalidSelection => "source_changed_or_invalid",
            CompactionError::InvalidCheckpoint => "checkpoint_invalid",
            CompactionError::Serialization => "serialization",
            CompactionError::Responses(_) => "response_storage",
        },
        _ => "database_other",
    }
}

pub(in crate::web::responses) fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(in crate::web::responses) fn capture_logs(action: impl FnOnce()) -> String {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Writer(Arc<Mutex<Vec<u8>>>);

    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Writer(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, action);
    let captured = bytes.lock().unwrap().clone();
    String::from_utf8(captured).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_identity_is_catalog_owned_and_unknown_input_is_never_logged() {
        assert_eq!(model_id("gpt-oss-120b"), "gpt-oss-120b");
        assert_eq!(model_id("PRIVATE_CALLER_STRING"), "uncataloged");
    }

    #[test]
    fn database_classifier_discards_nested_untrusted_error_text() {
        let sensitive = "PRIVATE_DATABASE_VALUE";
        let error = DBError::QueryError(diesel::result::Error::DeserializationError(
            std::io::Error::other(sensitive).into(),
        ));
        assert_eq!(db_error_category(&error), "database_query");
        assert!(!db_error_category(&error).contains(sensitive));
        assert_eq!(
            db_error_category(&CompactionError::InvalidSelection.into()),
            "source_changed_or_invalid"
        );
    }
}
