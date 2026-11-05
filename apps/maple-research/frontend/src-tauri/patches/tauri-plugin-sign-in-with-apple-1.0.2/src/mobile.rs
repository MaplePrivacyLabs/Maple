use serde::de::DeserializeOwned;
use tauri::{
  plugin::{PluginApi, PluginHandle},
  AppHandle, Runtime,
};

use crate::models::*;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_sign_in_with_apple);

// initializes the Kotlin or Swift plugin classes
pub fn init<R: Runtime, C: DeserializeOwned>(
  _app: &AppHandle<R>,
  api: PluginApi<R, C>,
) -> crate::Result<SignInWithApple<R>> {
  #[cfg(target_os = "android")]
  let handle = api.register_android_plugin("ch.manaf.tauri_plugins.sign_in_with_apple", "ExamplePlugin")?;
  #[cfg(target_os = "ios")]
  let handle = api.register_ios_plugin(init_plugin_sign_in_with_apple)?;
  Ok(SignInWithApple(handle))
}

/// Access to the sign-in-with-apple APIs.
pub struct SignInWithApple<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> SignInWithApple<R> {
  pub fn get_apple_id_credential(&self, payload: AppleIDAuthorizationRequest) -> crate::Result<AppleIDAuthorizationResponse> {
    self
      .0
      .run_mobile_plugin("get_apple_id_credential", payload)
      .map_err(Into::into)
  }
}
