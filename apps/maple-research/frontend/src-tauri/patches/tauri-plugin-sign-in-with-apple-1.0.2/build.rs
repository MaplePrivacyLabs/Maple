const COMMANDS: &[&str] = &["get_apple_id_credential"];

fn main() {
  tauri_plugin::Builder::new(COMMANDS)
    .android_path("android")
    .ios_path("ios")
    .build();
}
