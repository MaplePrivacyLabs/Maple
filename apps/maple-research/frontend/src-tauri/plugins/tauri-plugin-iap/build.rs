const COMMANDS: &[&str] = &[
    "register_listener",
    "remove_listener",
    "get_products",
    "purchase",
    "get_signed_transactions",
    "finish_transaction",
    "sync",
    "get_storefront",
    "manage_subscriptions",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).ios_path("ios").build();
}
