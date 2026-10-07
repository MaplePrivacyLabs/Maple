use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("CBOR error: {0}")]
    Cbor(String),

    #[error("Cryptographic error: {0}")]
    Crypto(String),

    #[error("Attestation verification failed: {0}")]
    AttestationVerificationFailed(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Key exchange failed: {0}")]
    KeyExchange(String),

    #[error("Encryption error: {0}")]
    Encryption(String),

    #[error("Decryption error: {0}")]
    Decryption(String),

    #[error("Authentication error: {0}")]
    Authentication(String),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("API error: {status}: {message}")]
    Api { status: u16, message: String },

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("UTF-8 conversion error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("Base64 decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    #[error("Other error: {0}")]
    Other(String),
}

impl Error {
    /// The HTTP status of an [`Error::Api`].
    pub fn api_status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The backend's machine-readable code when an [`Error::Api`] body carries
    /// one as `{"error":{"code":"..."}}`: `system_one_*` validation codes,
    /// `usage_limit_reached`, `model_not_available_on_plan` and the like.
    pub fn api_error_code(&self) -> Option<String> {
        let Self::Api { message, .. } = self else {
            return None;
        };
        let body: serde_json::Value = serde_json::from_str(message).ok()?;
        body.get("error")?.get("code")?.as_str().map(str::to_owned)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_code_reads_the_backend_error_object() {
        let error = Error::Api {
            status: 422,
            message: r#"{"status":422,"message":"At most 64 questions","error":{"message":"At most 64 questions","code":"system_one_too_many_questions"}}"#.to_string(),
        };
        assert_eq!(error.api_status(), Some(422));
        assert_eq!(
            error.api_error_code().as_deref(),
            Some("system_one_too_many_questions")
        );

        let legacy = Error::Api {
            status: 400,
            message: r#"{"status":400,"message":"Bad Request"}"#.to_string(),
        };
        assert_eq!(legacy.api_status(), Some(400));
        assert_eq!(legacy.api_error_code(), None);

        assert_eq!(Error::Other("x".to_string()).api_status(), None);
    }
}
