use tauri::{AppHandle, command, Runtime};

use crate::models::*;
use crate::Result;
use crate::SignInWithAppleExt;

#[command]
pub(crate) async fn get_apple_id_credential<R: Runtime>(
    app: AppHandle<R>,
    payload: AppleIDAuthorizationRequest,
) -> Result<AppleIDAuthorizationResponse> {
    app.sign_in_with_apple().get_apple_id_credential(payload)
}
