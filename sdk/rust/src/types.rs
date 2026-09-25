use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

// Attestation & Key Exchange Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationRequest {
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationResponse {
    pub attestation_document: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeRequest {
    pub client_public_key: String,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeResponse {
    pub encrypted_session_key: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedRequest {
    pub encrypted: String, // Base64-encoded (nonce + ciphertext)
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: Uuid,
    pub session_key: [u8; 32],
}

// Token Management Types
#[derive(Debug, Clone)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialUpdateResponse {
    #[serde(default)]
    pub message: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

// Auth Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub password: String,
    pub client_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub password: String,
    pub client_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub id: Uuid,
    pub email: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogoutRequest {
    pub refresh_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_device_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedResponse<T> {
    pub encrypted: String,
    #[serde(skip)]
    _phantom: std::marker::PhantomData<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NullableField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> NullableField<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub fn null() -> Self {
        Self::Null
    }

    pub fn value(value: T) -> Self {
        Self::Value(value)
    }
}

impl<T> Serialize for NullableField<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Missing | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for NullableField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

// OAuth Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthInitRequest {
    pub client_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthResponse {
    pub auth_url: String,
    #[serde(alias = "csrf_token")]
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleAuthResponse {
    pub auth_url: String,
    #[serde(alias = "csrf_token")]
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppleAuthResponse {
    pub auth_url: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCallbackRequest {
    pub code: String,
    pub state: String,
    pub invite_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppleNativeSignInRequest {
    pub user_identifier: String,
    pub identity_token: String,
    pub client_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<String>,
}

// User Profile Types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMethod {
    Email,
    Github,
    Google,
    Apple,
    Guest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUser {
    pub id: Uuid,
    pub name: Option<String>,
    pub email: Option<String>,
    pub email_verified: bool,
    pub login_method: LoginMethod,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserResponse {
    pub user: AppUser,
}

// Push Notification Types
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PushPlatform {
    Ios,
    Android,
}

impl PushPlatform {
    pub fn provider(self) -> PushProvider {
        match self {
            Self::Ios => PushProvider::Apns,
            Self::Android => PushProvider::Fcm,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PushProvider {
    Apns,
    Fcm,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PushEnvironment {
    Dev,
    #[default]
    Prod,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PushKeyAlgorithm {
    P256EcdhV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterPushDeviceRequest {
    pub installation_id: Uuid,
    pub platform: PushPlatform,
    pub provider: PushProvider,
    pub environment: PushEnvironment,
    pub app_id: String,
    pub push_token: String,
    pub notification_public_key: String,
    pub key_algorithm: PushKeyAlgorithm,
    #[serde(default)]
    pub supports_encrypted_preview: bool,
    #[serde(default)]
    pub supports_background_processing: bool,
}

impl RegisterPushDeviceRequest {
    pub fn new(
        installation_id: Uuid,
        platform: PushPlatform,
        environment: PushEnvironment,
        app_id: impl Into<String>,
        push_token: impl Into<String>,
        notification_public_key: impl Into<String>,
    ) -> Self {
        Self {
            installation_id,
            platform,
            provider: platform.provider(),
            environment,
            app_id: app_id.into(),
            push_token: push_token.into(),
            notification_public_key: notification_public_key.into(),
            key_algorithm: PushKeyAlgorithm::P256EcdhV1,
            supports_encrypted_preview: false,
            supports_background_processing: false,
        }
    }

    pub fn supports_encrypted_preview(mut self, supports_encrypted_preview: bool) -> Self {
        self.supports_encrypted_preview = supports_encrypted_preview;
        self
    }

    pub fn supports_background_processing(mut self, supports_background_processing: bool) -> Self {
        self.supports_background_processing = supports_background_processing;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushDevice {
    pub id: Uuid,
    pub object: String,
    pub installation_id: Uuid,
    pub platform: PushPlatform,
    pub provider: PushProvider,
    pub environment: PushEnvironment,
    pub app_id: String,
    pub key_algorithm: PushKeyAlgorithm,
    pub supports_encrypted_preview: bool,
    pub supports_background_processing: bool,
    pub last_seen_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushDeviceListResponse {
    pub object: String,
    pub data: Vec<PushDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeletedPushDeviceResponse {
    pub id: Uuid,
    pub object: String,
    pub deleted: bool,
}

// Private Key Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_derivation_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_phrase_derivation_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateKeyResponse {
    pub mnemonic: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateKeyBytesResponse {
    pub private_key: String, // Hex encoded (64 characters for 32 bytes)
}

// Message Signing Types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SigningAlgorithm {
    Schnorr,
    Ecdsa,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignMessageRequest {
    pub message_base64: String,
    pub algorithm: SigningAlgorithm,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_options: Option<SigningKeyOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningKeyOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_derivation_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_phrase_derivation_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignMessageResponse {
    pub signature: String,    // Base64 encoded
    pub message_hash: String, // Hex encoded
}

// Public Key Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicKeyResponse {
    pub public_key: String, // Hex encoded
    pub algorithm: SigningAlgorithm,
}

// Third Party Token Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThirdPartyTokenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThirdPartyTokenResponse {
    pub token: String,
}

// Encryption/Decryption Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptDataRequest {
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_options: Option<EncryptionKeyOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionKeyOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_derivation_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_phrase_derivation_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptDataResponse {
    pub encrypted_data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptDataRequest {
    pub encrypted_data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_options: Option<EncryptionKeyOptions>,
}

// The decrypted response is just a string, handled directly

// Account Management Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordResetRequest {
    pub email: String,
    pub hashed_secret: String,
    pub client_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordResetConfirmRequest {
    pub email: String,
    pub alphanumeric_code: String,
    pub plaintext_secret: String,
    pub new_password: String,
    pub client_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestVerificationCodeRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiateAccountDeletionRequest {
    pub hashed_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmAccountDeletionRequest {
    pub confirmation_code: String,
    pub plaintext_secret: String,
}

// API Key Management Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyListResponse {
    pub keys: Vec<ApiKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyCreateRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyCreateResponse {
    pub key: String, // UUID format with dashes, only returned on creation
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    pub pinned: bool,
    pub created_at: i64,
    pub last_activity_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationUpdateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "NullableField::is_missing")]
    pub project_id: NullableField<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

impl ConversationUpdateRequest {
    pub fn is_empty(&self) -> bool {
        self.metadata.is_none() && self.project_id.is_missing() && self.pinned.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationsListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unassigned_project: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationsListResponse {
    pub object: String,
    pub data: Vec<Conversation>,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationsDeleteResponse {
    pub object: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDeleteConversationsRequest {
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDeleteItemResult {
    pub id: String,
    pub object: String,
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDeleteConversationsResponse {
    pub object: String,
    pub data: Vec<BatchDeleteItemResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchUpdateConversationProjectRequest {
    pub ids: Vec<Uuid>,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchUpdateConversationProjectResponse {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationProject {
    pub id: Uuid,
    pub object: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationProjectListItem {
    pub id: Uuid,
    pub object: String,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationProjectsListResponse {
    pub object: String,
    pub data: Vec<ConversationProjectListItem>,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationProjectCreateRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationProjectUpdateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "NullableField::is_missing")]
    pub instructions: NullableField<String>,
}

impl ConversationProjectUpdateRequest {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.instructions.is_missing()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationProjectListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
}

// AI/OpenAI API Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default = "default_model_object")]
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// A catalog entry: model identity plus context and capability metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default = "default_model_object")]
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CatalogCapabilities>,
}

/// Capability flags a catalog entry advertises.
///
/// Mirrors the server and TypeScript SDK `ModelCapabilities` contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogCapabilities {
    #[serde(default)]
    pub chat: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub tool_use: bool,
}

/// A catalog alias resolving to a concrete model id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogAlias {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CatalogCapabilities>,
}

/// Default alias assignments from the catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub powerful: Option<String>,
}

/// The full model catalog from `/v1/models/catalog`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCatalogResponse {
    pub object: String,
    pub data: Vec<CatalogModel>,
    #[serde(default)]
    pub aliases: Vec<CatalogAlias>,
    #[serde(default)]
    pub defaults: CatalogDefaults,
}

fn default_model_object() -> String {
    "model".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<Model>,
}

// Tool Calling Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: Function,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionCall,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Value, // Now accepts both string and array formats
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: i32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

// Streaming types - transparent Value wrapper for full passthrough of any backend JSON.
// This avoids deserialization failures when LLMs send null fields in streaming tool_call
// deltas or introduce new fields the SDK doesn't know about yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatCompletionChunk(pub Value);

// Embeddings Types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub input: EmbeddingInput,
    #[serde(default = "default_embedding_model")]
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

fn default_embedding_model() -> String {
    "nomic-embed-text".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Multiple(Vec<String>),
}

impl From<String> for EmbeddingInput {
    fn from(s: String) -> Self {
        EmbeddingInput::Single(s)
    }
}

impl From<&str> for EmbeddingInput {
    fn from(s: &str) -> Self {
        EmbeddingInput::Single(s.to_string())
    }
}

impl From<Vec<String>> for EmbeddingInput {
    fn from(v: Vec<String>) -> Self {
        EmbeddingInput::Multiple(v)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

/// An embedding vector in the representation requested by `encoding_format`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Float(Vec<f64>),
    Base64(String),
}

impl EmbeddingVector {
    pub fn as_floats(&self) -> Option<&[f64]> {
        match self {
            Self::Float(values) => Some(values),
            Self::Base64(_) => None,
        }
    }

    pub fn as_base64(&self) -> Option<&str> {
        match self {
            Self::Float(_) => None,
            Self::Base64(value) => Some(value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: i32,
    pub embedding: EmbeddingVector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: i32,
    pub total_tokens: i32,
}

// Web API Types

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchWorkflow {
    #[default]
    Search,
    Images,
    Videos,
    News,
    Podcasts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchTimeRelative {
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchLens {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sites_included: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sites_excluded: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords_included: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords_excluded: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_relative: Option<WebSearchTimeRelative>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_region: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
    /// Search result class. The server defaults to [`WebSearchWorkflow::Search`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WebSearchWorkflow>,
    /// One-based result page, from 1 through 10.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u8>,
    /// Maximum results to return, from 1 through 50. The server defaults to 10.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
    /// Whether to omit potentially unsafe content. The server defaults to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_search: Option<bool>,
    /// Search collection timeout in seconds, from 0.5 through 4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens: Option<WebSearchLens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<WebSearchFilters>,
}

impl WebSearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            workflow: None,
            page: None,
            limit: None,
            safe_search: None,
            timeout: None,
            lens_id: None,
            lens: None,
            filters: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub category: String,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub results: Vec<WebSearchResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebExtractRequest {
    /// Public HTTPS URLs to extract, from 1 through 10 entries.
    pub urls: Vec<String>,
    /// Bulk extraction timeout in seconds, from 0.5 through 10.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

impl WebExtractRequest {
    pub fn new(urls: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            urls: urls.into_iter().map(Into::into).collect(),
            timeout: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebExtractPageError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebExtractPage {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WebExtractPageError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebExtractResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub pages: Vec<WebExtractPage>,
}

// Agent API Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentChatRequest {
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct InitMainAgentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitMainAgentResponse {
    pub id: Uuid,
    pub object: String,
    pub kind: String,
    pub conversation_id: Uuid,
    pub display_name: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<ConversationItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSubagentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub purpose: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetMessageReactionRequest {
    pub emoji: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainAgentResponse {
    pub id: Uuid,
    pub object: String,
    pub kind: String,
    pub conversation_id: Uuid,
    pub display_name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResponse {
    pub id: Uuid,
    pub object: String,
    pub kind: String,
    pub conversation_id: Uuid,
    pub display_name: String,
    pub purpose: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentItemsListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListSubagentsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
    #[serde(rename = "input_file")]
    InputFile { filename: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReasoningContentItem {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationItem {
    #[serde(rename = "message")]
    Message {
        id: Uuid,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        role: String,
        content: Vec<ConversationContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reaction: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
    #[serde(rename = "function_call")]
    FunctionToolCall {
        id: Uuid,
        call_id: Uuid,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
    #[serde(rename = "function_call_output")]
    FunctionToolCallOutput {
        id: Uuid,
        call_id: Uuid,
        output: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: Uuid,
        content: Vec<ReasoningContentItem>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentItemsListResponse {
    pub object: String,
    pub data: Vec<ConversationItem>,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
    pub has_more: bool,
}

pub type ConversationItemsResponse = AgentItemsListResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentListResponse {
    pub object: String,
    pub data: Vec<SubagentResponse>,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedObjectResponse {
    pub id: Uuid,
    pub object: String,
    pub deleted: bool,
}

// Agent SSE event types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessageEvent {
    pub message_id: Uuid,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReactionEvent {
    pub item_id: Uuid,
    pub emoji: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypingEvent {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDoneEvent {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentErrorEvent {
    pub error: String,
}

/// Parsed SSE event from the agent chat stream
#[derive(Debug, Clone)]
pub enum AgentSseEvent {
    Message(AgentMessageEvent),
    Reaction(AgentReactionEvent),
    Typing(AgentTypingEvent),
    Done(AgentDoneEvent),
    Error(AgentErrorEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nullable_field_request_serialization_distinguishes_missing_and_null() {
        let conversation_update = ConversationUpdateRequest::default();
        assert!(conversation_update.is_empty());
        assert_eq!(
            serde_json::to_value(&conversation_update).unwrap(),
            json!({})
        );

        let conversation_update = ConversationUpdateRequest {
            project_id: NullableField::null(),
            ..Default::default()
        };
        assert!(!conversation_update.is_empty());
        assert_eq!(
            serde_json::to_value(&conversation_update).unwrap(),
            json!({ "project_id": null })
        );

        let project_update = ConversationProjectUpdateRequest {
            instructions: NullableField::null(),
            ..Default::default()
        };
        assert!(!project_update.is_empty());
        assert_eq!(
            serde_json::to_value(&project_update).unwrap(),
            json!({ "instructions": null })
        );
    }

    #[test]
    fn batch_update_project_request_serializes_none_as_explicit_null() {
        let request = BatchUpdateConversationProjectRequest {
            ids: vec![Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()],
            project_id: None,
        };

        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "ids": ["550e8400-e29b-41d4-a716-446655440000"],
                "project_id": null
            })
        );
    }

    #[test]
    fn credential_update_response_tolerates_missing_message() {
        let response: CredentialUpdateResponse =
            serde_json::from_value(json!({ "access_token": "new-access" })).unwrap();

        assert_eq!(response.message, "");
        assert_eq!(response.access_token.as_deref(), Some("new-access"));
        assert_eq!(response.refresh_token, None);
    }

    #[test]
    fn web_requests_omit_unspecified_options() {
        let search = WebSearchRequest::new("maple privacy");
        assert_eq!(
            serde_json::to_value(search).unwrap(),
            json!({ "query": "maple privacy" })
        );

        let extract = WebExtractRequest::new(["https://example.com/article"]);
        assert_eq!(
            serde_json::to_value(extract).unwrap(),
            json!({ "urls": ["https://example.com/article"] })
        );
    }

    #[test]
    fn web_responses_tolerate_absent_optional_fields() {
        let search: WebSearchResponse = serde_json::from_value(json!({
            "results": [{
                "category": "search",
                "url": "https://example.com",
                "title": "Example"
            }]
        }))
        .unwrap();
        assert_eq!(search.trace_id, None);
        assert_eq!(search.results[0].snippet, None);
        assert_eq!(search.results[0].published_at, None);

        let extract: WebExtractResponse = serde_json::from_value(json!({
            "pages": [{ "url": "https://example.com" }]
        }))
        .unwrap();
        assert_eq!(extract.trace_id, None);
        assert_eq!(extract.pages[0].markdown, None);
        assert_eq!(extract.pages[0].error, None);
    }

    #[test]
    fn embedding_response_deserializes_float_vectors() {
        let response: EmbeddingResponse = serde_json::from_value(json!({
            "object": "list",
            "data": [{
                "object": "embedding",
                "index": 0,
                "embedding": [0.1, 0.2, 0.3]
            }],
            "model": "nomic-embed-text",
            "usage": { "prompt_tokens": 4, "total_tokens": 4 }
        }))
        .unwrap();

        assert_eq!(
            response.data[0].embedding,
            EmbeddingVector::Float(vec![0.1, 0.2, 0.3])
        );
        assert_eq!(
            response.data[0].embedding.as_floats(),
            Some([0.1, 0.2, 0.3].as_slice())
        );
        assert_eq!(response.data[0].embedding.as_base64(), None);
        assert_eq!(
            serde_json::to_value(&response).unwrap()["data"][0]["embedding"],
            json!([0.1, 0.2, 0.3])
        );
    }

    #[test]
    fn embedding_response_deserializes_base64_vectors() {
        let response: EmbeddingResponse = serde_json::from_value(json!({
            "object": "list",
            "data": [{
                "object": "embedding",
                "index": 0,
                "embedding": "AQIDBA=="
            }],
            "model": "nomic-embed-text",
            "usage": { "prompt_tokens": 4, "total_tokens": 4 }
        }))
        .unwrap();

        assert_eq!(
            response.data[0].embedding,
            EmbeddingVector::Base64("AQIDBA==".to_string())
        );
        assert_eq!(response.data[0].embedding.as_floats(), None);
        assert_eq!(response.data[0].embedding.as_base64(), Some("AQIDBA=="));
        assert_eq!(
            serde_json::to_value(&response).unwrap()["data"][0]["embedding"],
            json!("AQIDBA==")
        );
    }

    #[test]
    fn embedding_response_rejects_unknown_vector_representations() {
        let response = serde_json::from_value::<EmbeddingResponse>(json!({
            "object": "list",
            "data": [{
                "object": "embedding",
                "index": 0,
                "embedding": { "values": [0.1, 0.2, 0.3] }
            }],
            "model": "nomic-embed-text",
            "usage": { "prompt_tokens": 4, "total_tokens": 4 }
        }));

        assert!(response.is_err());
    }
}
