use crate::{
    IapExt, ProductsResponse, PurchaseOutcome, Result, SignedTransactionsResponse,
    StorefrontResponse,
};
use tauri::{AppHandle, Runtime, command};

#[command]
pub async fn get_products<R: Runtime>(
    app: AppHandle<R>,
    product_ids: Vec<String>,
) -> Result<ProductsResponse> {
    app.iap().get_products(product_ids).await
}
#[command]
pub async fn purchase<R: Runtime>(
    app: AppHandle<R>,
    product_id: String,
    app_account_token: String,
) -> Result<PurchaseOutcome> {
    app.iap().purchase(product_id, app_account_token).await
}
#[command]
pub async fn get_signed_transactions<R: Runtime>(
    app: AppHandle<R>,
) -> Result<SignedTransactionsResponse> {
    app.iap().get_signed_transactions().await
}
#[command]
pub async fn finish_transaction<R: Runtime>(
    app: AppHandle<R>,
    transaction_id: String,
) -> Result<()> {
    app.iap().finish_transaction(transaction_id).await
}
#[command]
pub async fn sync<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.iap().sync().await
}
#[command]
pub async fn get_storefront<R: Runtime>(app: AppHandle<R>) -> Result<StorefrontResponse> {
    app.iap().get_storefront().await
}
#[command]
pub async fn manage_subscriptions<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.iap().manage_subscriptions().await
}
