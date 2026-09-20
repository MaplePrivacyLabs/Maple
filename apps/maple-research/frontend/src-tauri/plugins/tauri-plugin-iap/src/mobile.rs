use crate::{Error, Result, models::*};
use serde::de::DeserializeOwned;
use tauri::{
    AppHandle, Runtime,
    plugin::{PluginApi, PluginHandle},
};

tauri::ios_plugin_binding!(init_plugin_iap);

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<Iap<R>> {
    Ok(Iap(api.register_ios_plugin(init_plugin_iap)?))
}

pub struct Iap<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> Iap<R> {
    pub async fn get_products(&self, product_ids: Vec<String>) -> Result<ProductsResponse> {
        self.0
            .run_mobile_plugin_async("getProducts", ProductsRequest { product_ids })
            .await
            .map_err(Into::into)
    }
    pub async fn purchase(
        &self,
        product_id: String,
        app_account_token: String,
    ) -> Result<PurchaseOutcome> {
        self.0
            .run_mobile_plugin_async(
                "purchase",
                PurchaseRequest {
                    product_id,
                    app_account_token,
                },
            )
            .await
            .map_err(Into::into)
    }
    pub async fn get_signed_transactions(&self) -> Result<SignedTransactionsResponse> {
        self.0
            .run_mobile_plugin_async("getSignedTransactions", serde_json::json!({}))
            .await
            .map_err(Into::into)
    }
    pub async fn finish_transaction(&self, transaction_id: String) -> Result<()> {
        if !valid_transaction_id(&transaction_id) {
            return Err(Error::InvalidTransactionId);
        }
        self.0
            .run_mobile_plugin_async("finishTransaction", FinishRequest { transaction_id })
            .await
            .map_err(Into::into)
    }
    pub async fn sync(&self) -> Result<()> {
        self.0
            .run_mobile_plugin_async("sync", serde_json::json!({}))
            .await
            .map_err(Into::into)
    }
    pub async fn get_storefront(&self) -> Result<StorefrontResponse> {
        self.0
            .run_mobile_plugin_async("getStorefront", serde_json::json!({}))
            .await
            .map_err(Into::into)
    }
    pub async fn manage_subscriptions(&self) -> Result<()> {
        self.0
            .run_mobile_plugin_async("manageSubscriptions", serde_json::json!({}))
            .await
            .map_err(Into::into)
    }
}
