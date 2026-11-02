use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppleIDAuthorizationRequest {
  pub scope: Vec<String>,
  pub nonce: Option<String>,
  pub state: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppleIDAuthorizationResponse {
  pub user_identifier: Option<String>,

  pub given_name: Option<String>,
  pub family_name: Option<String>,
  pub email: Option<String>,

  pub authorization_code: String,
  pub identity_token: Option<String>,
  pub state: Option<String>,
}


#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingRequest {
  pub value: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingResponse {
  pub value: Option<String>,
}
