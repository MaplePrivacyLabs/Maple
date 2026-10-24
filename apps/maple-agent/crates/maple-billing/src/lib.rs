//! HTTP client for the Maple billing API. Mirrors
//! `frontend/src/billing/billingApi.ts` in the Maple web app.
//!
//! The client takes a third-party JWT as a plain string. Mint it with the
//! OpenSecret SDK (`generate_third_party_token`) with the billing base URL
//! as the audience. This crate does not depend on the enclave client.

use serde::{Deserialize, Serialize};

pub const DEFAULT_BILLING_API_URL: &str = "https://billing.opensecret.cloud";

/// `GET /v1/maple/subscription/status`. Every field is optional on the
/// wire except the booleans, which default to false when absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BillingStatus {
    #[serde(default)]
    pub is_subscribed: bool,
    #[serde(default)]
    pub stripe_customer_id: Option<String>,
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub product_name: Option<String>,
    /// Stripe-style status: `active`, `trialing`, `past_due`, `canceled`…
    #[serde(default)]
    pub subscription_status: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub current_period_end: Option<i64>,
    #[serde(default)]
    pub can_chat: bool,
    #[serde(default)]
    pub chats_remaining: Option<i64>,
    /// `stripe`, `zaprite`, or `subscription_pass`.
    #[serde(default)]
    pub payment_provider: Option<String>,
    #[serde(default)]
    pub total_tokens: Option<i64>,
    #[serde(default)]
    pub used_tokens: Option<i64>,
    #[serde(default)]
    pub usage_reset_date: Option<String>,
    #[serde(default)]
    pub api_credit_balance: Option<i64>,
}

/// One purchasable plan from `GET /v1/maple/products` (public).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Product {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub default_price: Option<Price>,
    /// `Some(false)` when the store build may not sell it.
    #[serde(default)]
    pub is_available: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Price {
    pub id: String,
    pub currency: String,
    /// Minor units (cents for USD).
    pub unit_amount: i64,
    #[serde(default)]
    pub recurring: Option<Recurring>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recurring {
    pub interval: String,
    #[serde(default)]
    pub interval_count: Option<i64>,
}

/// `POST /v1/maple/subscription/checkout`.
#[derive(Debug, Clone, Serialize)]
pub struct CheckoutRequest {
    pub email: String,
    pub product_id: String,
    pub success_url: String,
    pub cancel_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<u64>,
}

/// Prepaid API credits: $1 buys 1,000 credits, and one purchase is $10 to
/// $1,000, matching the Maple web app's credit packages.
pub const CREDITS_PER_DOLLAR: u64 = 1_000;
pub const MIN_CREDIT_PURCHASE_DOLLARS: u64 = 10;
pub const MAX_CREDIT_PURCHASE_DOLLARS: u64 = 1_000;

/// How a credit purchase is paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethod {
    /// Card checkout through Stripe.
    Stripe,
    /// Bitcoin checkout through Zaprite.
    Zaprite,
}

/// `POST /v1/maple/api-credits/purchase` and `…/purchase-zaprite`. Zaprite
/// has no cancel page, so it takes no `cancel_url`.
#[derive(Debug, Clone, Serialize)]
pub struct CreditPurchaseRequest {
    pub credits: u64,
    /// Empty for guest accounts: the server reads the account from the token.
    pub email: String,
    pub success_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_url: Option<String>,
}

#[derive(Debug)]
pub enum BillingError {
    /// The third-party token was rejected; mint a new one and retry.
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for BillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("billing API rejected the session token"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BillingError {}

#[derive(Debug, Clone)]
pub struct BillingClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct PortalResponse {
    portal_url: String,
}

#[derive(Deserialize)]
struct CheckoutResponse {
    checkout_url: String,
}

#[derive(Deserialize)]
struct BalanceResponse {
    balance: i64,
}

#[derive(Serialize)]
struct PortalRequest<'a> {
    return_url: &'a str,
}

impl BillingClient {
    /// Build a client. A trailing slash on `base_url` is removed.
    ///
    /// Fails when the HTTP client cannot be built (for example when the TLS
    /// backend is unavailable). `reqwest::Client::default()` panics in the
    /// same conditions, so there is no silent fallback.
    pub fn new(base_url: impl Into<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|error| format!("failed to build the billing HTTP client: {error}"))?;
        let base_url = base_url.into().trim().trim_end_matches('/').to_string();
        Ok(Self { base_url, http })
    }

    /// The base URL, also the audience for the third-party token.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn subscription_status(&self, token: &str) -> Result<BillingStatus, BillingError> {
        self.send(
            "subscription status",
            self.http
                .get(format!("{}/v1/maple/subscription/status", self.base_url))
                .bearer_auth(token),
        )
        .await
    }

    /// The plans on sale. No token: the route is public.
    pub async fn products(&self) -> Result<Vec<Product>, BillingError> {
        self.send(
            "products",
            self.http
                .get(format!("{}/v1/maple/products", self.base_url)),
        )
        .await
    }

    /// A Stripe customer portal URL that returns to `return_url`.
    pub async fn portal_url(&self, token: &str, return_url: &str) -> Result<String, BillingError> {
        let response: PortalResponse = self
            .send(
                "portal",
                self.http
                    .post(format!("{}/v1/maple/subscription/portal", self.base_url))
                    .bearer_auth(token)
                    .json(&PortalRequest { return_url }),
            )
            .await?;
        Ok(response.portal_url)
    }

    /// A Stripe checkout URL for `request.product_id`.
    pub async fn checkout_url(
        &self,
        token: &str,
        request: &CheckoutRequest,
    ) -> Result<String, BillingError> {
        let response: CheckoutResponse = self
            .send(
                "checkout",
                self.http
                    .post(format!("{}/v1/maple/subscription/checkout", self.base_url))
                    .bearer_auth(token)
                    .json(request),
            )
            .await?;
        Ok(response.checkout_url)
    }

    /// A Stripe or Zaprite checkout URL for prepaid API credits.
    pub async fn credit_checkout_url(
        &self,
        token: &str,
        method: PaymentMethod,
        request: &CreditPurchaseRequest,
    ) -> Result<String, BillingError> {
        let path = match method {
            PaymentMethod::Stripe => "purchase",
            PaymentMethod::Zaprite => "purchase-zaprite",
        };
        let response: CheckoutResponse = self
            .send(
                "credit checkout",
                self.http
                    .post(format!("{}/v1/maple/api-credits/{path}", self.base_url))
                    .bearer_auth(token)
                    .json(request),
            )
            .await?;
        Ok(response.checkout_url)
    }

    /// Prepaid API credits left on the account.
    pub async fn api_credit_balance(&self, token: &str) -> Result<i64, BillingError> {
        let response: BalanceResponse = self
            .send(
                "credit balance",
                self.http
                    .get(format!("{}/v1/maple/api-credits/balance", self.base_url))
                    .bearer_auth(token),
            )
            .await?;
        Ok(response.balance)
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        what: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<T, BillingError> {
        let response = request
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|error| BillingError::Other(format!("billing request failed: {error}")))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BillingError::Unauthorized);
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(BillingError::Other(format!(
                "billing {what} failed ({status}): {body}"
            )));
        }
        response
            .json::<T>()
            .await
            .map_err(|error| BillingError::Other(format!("billing {what} decode failed: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_slash() {
        assert_eq!(
            BillingClient::new("https://billing.example/ ")
                .unwrap()
                .base_url(),
            "https://billing.example"
        );
    }

    #[test]
    fn decodes_partial_status() {
        let status: BillingStatus =
            serde_json::from_str(r#"{"product_name":"Max","is_subscribed":true}"#).unwrap();
        assert_eq!(status.product_name.as_deref(), Some("Max"));
        assert!(status.is_subscribed);
        assert_eq!(status.total_tokens, None);
        assert_eq!(status.current_period_end, None);
    }

    #[test]
    fn decodes_a_full_status_and_products() {
        let status: BillingStatus = serde_json::from_str(
            r#"{"is_subscribed":true,"stripe_customer_id":"cus_1","product_id":"prod_1",
                "product_name":"Pro","subscription_status":"active","current_period_end":1790000000,
                "can_chat":true,"chats_remaining":null,"payment_provider":"stripe",
                "total_tokens":20000,"used_tokens":5,"usage_reset_date":"2026-10-01T00:00:00Z",
                "api_credit_balance":100000}"#,
        )
        .unwrap();
        assert_eq!(status.current_period_end, Some(1790000000));
        assert_eq!(status.api_credit_balance, Some(100000));
        let products: Vec<Product> = serde_json::from_str(
            r#"[{"id":"prod_1","name":"Pro","active":true,
                 "default_price":{"id":"price_1","currency":"usd","unit_amount":2000,
                                  "recurring":{"interval":"month","interval_count":1}}}]"#,
        )
        .unwrap();
        assert_eq!(
            products[0].default_price.as_ref().unwrap().unit_amount,
            2000
        );
        assert_eq!(products[0].is_available, None);
    }

    #[test]
    fn checkout_request_omits_quantity_when_absent() {
        let body = serde_json::to_string(&CheckoutRequest {
            email: "a@b.c".into(),
            product_id: "prod".into(),
            success_url: "https://s".into(),
            cancel_url: "https://c".into(),
            quantity: None,
        })
        .unwrap();
        assert!(!body.contains("quantity"));
    }

    #[test]
    fn credit_purchase_omits_cancel_url_for_zaprite() {
        let mut request = CreditPurchaseRequest {
            credits: 20_000,
            email: String::new(),
            success_url: "https://s".into(),
            cancel_url: None,
        };
        let body: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"credits": 20000, "email": "", "success_url": "https://s"})
        );
        request.cancel_url = Some("https://c".into());
        assert_eq!(
            serde_json::to_value(&request).unwrap()["cancel_url"],
            "https://c"
        );
        assert_eq!(
            serde_json::to_value(PaymentMethod::Zaprite).unwrap(),
            "zaprite"
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "hits the live billing API"]
    async fn dummy_token_is_rejected_quickly() {
        let client = BillingClient::new(DEFAULT_BILLING_API_URL).unwrap();
        let started = std::time::Instant::now();
        let result = client.subscription_status("dummy").await;
        eprintln!("result after {:?}: {result:?}", started.elapsed());
        assert!(matches!(result, Err(BillingError::Unauthorized)));
    }
}
