#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Mobile(#[from] tauri::plugin::mobile::PluginInvokeError),
    #[error("invalid_transaction_id")]
    InvalidTransactionId,
}

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
