use serde::de::DeserializeOwned;
use tauri::{plugin::PluginApi, AppHandle, Runtime};

use crate::models::*;

pub fn init<R: Runtime, C: DeserializeOwned>(
  app: &AppHandle<R>,
  _api: PluginApi<R, C>,
) -> crate::Result<SignInWithApple<R>> {
  Ok(SignInWithApple(app.clone()))
}

/// Access to the sign-in-with-apple APIs.
pub struct SignInWithApple<R: Runtime>(AppHandle<R>);

impl<R: Runtime> SignInWithApple<R> {
  pub fn get_apple_id_credential(&self, _payload: AppleIDAuthorizationRequest) -> crate::Result<AppleIDAuthorizationResponse> {
    Err(crate::Error::UnsupportedPlatformError)
  }
}
