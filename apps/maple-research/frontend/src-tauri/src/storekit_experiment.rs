/// The local StoreKit lab is an explicitly compiled diagnostic entrypoint. It
/// must never replace Maple's normal UI in a release or physical-device build.
#[tauri::command]
pub fn storekit_experiment_enabled() -> bool {
    cfg!(all(debug_assertions, target_os = "ios", target_abi = "sim"))
}
