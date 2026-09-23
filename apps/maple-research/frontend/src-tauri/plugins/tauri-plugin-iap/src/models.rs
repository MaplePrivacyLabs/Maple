use serde::{Deserialize, Serialize};

// Intentionally no Debug: signed transactions are sensitive and must not be logged.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedTransaction {
    pub transaction_id: String,
    pub original_transaction_id: String,
    pub product_id: String,
    pub jws: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum PurchaseOutcome {
    Success { transaction: SignedTransaction },
    Pending,
    Cancelled,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedTransactionsResponse {
    pub transactions: Vec<SignedTransaction>,
    pub unfinished_transaction_ids: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorefrontResponse {
    pub country_code: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    pub product_id: String,
    pub title: String,
    pub description: String,
    pub formatted_price: String,
    pub price_currency_code: String,
    pub subscription_period: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ProductsResponse {
    pub products: Vec<Product>,
}

#[cfg(any(test, target_os = "ios"))]
pub(crate) fn valid_transaction_id(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|number| number.to_string() == value)
}

#[cfg(target_os = "ios")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProductsRequest {
    pub product_ids: Vec<String>,
}
#[cfg(target_os = "ios")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PurchaseRequest {
    pub product_id: String,
    pub app_account_token: String,
}
#[cfg(target_os = "ios")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FinishRequest {
    pub transaction_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_identifiers_preserve_full_unsigned_precision() {
        let response: SignedTransactionsResponse = serde_json::from_value(serde_json::json!({
            "transactions": [{"transactionId":"18446744073709551615", "originalTransactionId":"9007199254740993", "productId":"pro", "jws":"fixture"}],
            "unfinishedTransactionIds": ["18446744073709551615"]
        })).unwrap();
        assert_eq!(
            response.transactions[0].transaction_id,
            "18446744073709551615"
        );
        assert_eq!(
            serde_json::to_value(response).unwrap()["transactions"][0]["originalTransactionId"],
            "9007199254740993"
        );
        assert!(valid_transaction_id("18446744073709551615"));
        assert!(valid_transaction_id("0"));
        for invalid in [
            "01",
            "+1",
            "-1",
            "1.0",
            "1e2",
            " 1",
            "",
            "18446744073709551616",
        ] {
            assert!(!valid_transaction_id(invalid), "accepted {invalid}");
        }
    }

    #[test]
    fn pending_and_cancelled_do_not_require_or_fabricate_a_transaction() {
        for status in ["pending", "cancelled"] {
            let outcome: PurchaseOutcome =
                serde_json::from_value(serde_json::json!({"status":status})).unwrap();
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!({"status":status})
            );
        }
        assert!(
            serde_json::from_value::<PurchaseOutcome>(serde_json::json!({"status":"success"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<PurchaseOutcome>(serde_json::json!({"status":"unknown"}))
                .is_err()
        );
    }
}
