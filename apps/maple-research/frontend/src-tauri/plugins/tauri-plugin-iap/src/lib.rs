//! iOS-only StoreKit bridge. Server acknowledgement and account ownership belong
//! to the caller. This fork never finishes a transaction during delivery.

mod models;
pub use models::*;

#[cfg(target_os = "ios")]
mod commands;
#[cfg(target_os = "ios")]
mod error;
#[cfg(target_os = "ios")]
mod mobile;
#[cfg(target_os = "ios")]
pub use error::{Error, Result};
#[cfg(target_os = "ios")]
pub use mobile::Iap;

#[cfg(target_os = "ios")]
use tauri::Manager;
use tauri::{
    Runtime,
    plugin::{Builder, TauriPlugin},
};

#[cfg(target_os = "ios")]
pub trait IapExt<R: Runtime> {
    fn iap(&self) -> &Iap<R>;
}

#[cfg(target_os = "ios")]
impl<R: Runtime, T: Manager<R>> IapExt<R> for T {
    fn iap(&self) -> &Iap<R> {
        self.state::<Iap<R>>().inner()
    }
}

/// Register only on iOS. Non-iOS builds expose no commands or native service.
#[must_use]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    let builder = Builder::new("iap");
    #[cfg(target_os = "ios")]
    let builder = builder
        .invoke_handler(tauri::generate_handler![
            commands::get_products,
            commands::purchase,
            commands::get_signed_transactions,
            commands::finish_transaction,
            commands::sync,
            commands::get_storefront,
            commands::manage_subscriptions,
        ])
        .setup(|app, api| {
            app.manage(mobile::init(app, api)?);
            Ok(())
        });
    builder.build()
}
