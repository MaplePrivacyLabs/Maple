//! Request authentication contracts from Pi v1.0.4; credential storage is host-owned.
use crate::env::CancellationToken;
use crate::types::{ProviderEnv, ProviderHeaders};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct AuthOperationOptions {
    pub signal: Option<CancellationToken>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthResult {
    pub auth: ModelAuth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthCheck {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub r#type: AuthType,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    ApiKey,
    Oauth,
}
