//! Account usage history from the billing API. Mirrors
//! `frontend/src/billing/usageApi.ts` in the Maple web app.
//!
//! Credits are what billing charged at the time. Token counts are split so
//! input + cached input + output add up to the total. Every field defaults
//! when absent, as the web client does, so an older billing server that
//! omits `client_app` or `extra_credits` still decodes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{BillingClient, BillingError};

/// Labels for the model ids that appear in a response.
pub type UsageModelLabels = BTreeMap<String, UsageModelLabel>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageModelLabel {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Counts shared by day and month rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageCounts {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub uncached_input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Requests that reported a cached count. 0 means "not reported", not
    /// "none".
    #[serde(default)]
    pub cached_reported_requests: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub credits: f64,
}

/// One day × model × source × app × pool bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageDayRow {
    /// Local calendar date (`YYYY-MM-DD`) in the query's time zone.
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub model: String,
    /// How the request authenticated: `app` (a session) or `api` (an API key).
    #[serde(default)]
    pub source: String,
    /// The app that sent the request (`chat`, `work`, `proxy`), independent
    /// of how it authenticated. Absent for clients that reported none.
    #[serde(default)]
    pub client_app: Option<String>,
    /// `plan` or `extra`.
    #[serde(default)]
    pub pool: String,
    #[serde(flatten)]
    pub counts: UsageCounts,
}

/// One calendar month × source × app × pool bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageMonthRow {
    /// Local calendar month (`YYYY-MM`).
    #[serde(default)]
    pub month: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub client_app: Option<String>,
    #[serde(default)]
    pub pool: String,
    #[serde(flatten)]
    pub counts: UsageCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsagePeriodDay {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub plan_credits: f64,
    /// Absent from billing versions that report only the plan pool.
    #[serde(default)]
    pub extra_credits: f64,
}

/// Daily credits across the billing window the query named.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageCurrentPeriod {
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub days: Vec<UsagePeriodDay>,
}

/// The dates the server resolved the range to. Empty strings when absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRangeInfo {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub tz: String,
    #[serde(default)]
    pub start_date: String,
    #[serde(default)]
    pub end_date: String,
}

/// `GET /v1/maple/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    #[serde(default)]
    pub range: UsageRangeInfo,
    #[serde(default)]
    pub models: UsageModelLabels,
    #[serde(default)]
    pub days: Vec<UsageDayRow>,
    #[serde(default)]
    pub months: Vec<UsageMonthRow>,
    #[serde(default)]
    pub current_period: Option<UsageCurrentPeriod>,
}

/// The query for [`BillingClient::usage`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageQuery {
    /// `last_30_days` or `month:YYYY-MM`.
    pub range: String,
    /// IANA time zone the server buckets days in.
    pub tz: String,
    /// The status usage window; billing returns `current_period` for it
    /// only when both ends are set.
    #[serde(default)]
    pub period_start: Option<String>,
    #[serde(default)]
    pub period_end: Option<String>,
}

/// One request from `GET /v1/maple/usage/requests`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRequest {
    /// RFC 3339 instant.
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub client_app: Option<String>,
    #[serde(default)]
    pub pool: String,
    #[serde(default)]
    pub uncached_input_tokens: u64,
    /// `None` when the provider did not report a cached count.
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub credits: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRequestsPage {
    #[serde(default)]
    pub models: UsageModelLabels,
    #[serde(default)]
    pub requests: Vec<UsageRequest>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One model's current customer price, in credits per million tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsagePrice {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub input: f64,
    /// `None`: no cached discount, cached tokens bill as input.
    #[serde(default)]
    pub cached_input: Option<f64>,
    #[serde(default)]
    pub output: f64,
    /// The price depends on the provider that served the request.
    #[serde(default)]
    pub price_may_vary: bool,
}

/// `GET /v1/maple/usage/prices` (public).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsagePrices {
    #[serde(default = "default_credits_per_usd")]
    pub credits_per_usd: f64,
    #[serde(default = "default_price_unit")]
    pub unit: String,
    #[serde(default)]
    pub models: Vec<UsagePrice>,
}

fn default_credits_per_usd() -> f64 {
    1_000.0
}

fn default_price_unit() -> String {
    "credits_per_million_tokens".into()
}

impl BillingClient {
    /// Usage history for `query.range`, bucketed by day in `query.tz`.
    pub async fn usage(
        &self,
        token: &str,
        query: &UsageQuery,
    ) -> Result<UsageSummary, BillingError> {
        let url = self.usage_url(query)?;
        self.send("usage", self.http.get(url).bearer_auth(token))
            .await
    }

    /// Recent requests, newest first. `cursor` is a previous page's
    /// `next_cursor`.
    pub async fn usage_requests(
        &self,
        token: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<UsageRequestsPage, BillingError> {
        let url = self.usage_requests_url(cursor, limit)?;
        self.send("recent requests", self.http.get(url).bearer_auth(token))
            .await
    }

    /// Current model prices. No token: the route is public.
    pub async fn usage_prices(&self) -> Result<UsagePrices, BillingError> {
        let url = self.endpoint("/v1/maple/usage/prices")?;
        self.send("model prices", self.http.get(url)).await
    }

    fn endpoint(&self, path: &str) -> Result<reqwest::Url, BillingError> {
        reqwest::Url::parse(&format!("{}{path}", self.base_url))
            .map_err(|error| BillingError::Other(format!("invalid billing URL: {error}")))
    }

    fn usage_url(&self, query: &UsageQuery) -> Result<reqwest::Url, BillingError> {
        let mut url = self.endpoint("/v1/maple/usage")?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("range", &query.range);
            pairs.append_pair("tz", &query.tz);
            if let (Some(start), Some(end)) = (&query.period_start, &query.period_end) {
                pairs.append_pair("period_start", start);
                pairs.append_pair("period_end", end);
            }
        }
        Ok(url)
    }

    fn usage_requests_url(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<reqwest::Url, BillingError> {
        let mut url = self.endpoint("/v1/maple/usage/requests")?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("limit", &limit.to_string());
            if let Some(cursor) = cursor {
                pairs.append_pair("cursor", cursor);
            }
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> BillingClient {
        BillingClient::new("https://billing.example/").unwrap()
    }

    #[test]
    fn usage_query_encodes_the_range_zone_and_window_together() {
        let mut query = UsageQuery {
            range: "month:2026-09".into(),
            tz: "America/Chicago".into(),
            period_start: Some("2026-09-01T00:00:00Z".into()),
            period_end: None,
        };
        assert_eq!(
            client().usage_url(&query).unwrap().as_str(),
            "https://billing.example/v1/maple/usage?range=month%3A2026-09&tz=America%2FChicago"
        );
        query.period_end = Some("2026-10-01T00:00:00Z".into());
        assert_eq!(
            client().usage_url(&query).unwrap().query(),
            Some(
                "range=month%3A2026-09&tz=America%2FChicago\
                 &period_start=2026-09-01T00%3A00%3A00Z&period_end=2026-10-01T00%3A00%3A00Z"
            )
        );
    }

    #[test]
    fn requests_query_carries_the_cursor_only_after_the_first_page() {
        assert_eq!(
            client().usage_requests_url(None, 10).unwrap().query(),
            Some("limit=10")
        );
        assert_eq!(
            client()
                .usage_requests_url(Some("abc+/="), 50)
                .unwrap()
                .query(),
            Some("limit=50&cursor=abc%2B%2F%3D")
        );
    }

    #[test]
    fn usage_from_an_older_server_decodes_with_defaults() {
        // No client_app, no extra_credits, no cached counts, no range.
        let usage: UsageSummary = serde_json::from_str(
            r#"{"days":[{"date":"2026-10-02","model":"glm-5-3","source":"app","pool":"plan",
                         "requests":3,"uncached_input_tokens":900,"output_tokens":120,"credits":12.5}],
                "current_period":{"start":"2026-10-01T00:00:00Z","end":"2026-11-01T00:00:00Z",
                                  "days":[{"date":"2026-10-02","plan_credits":12.5}]}}"#,
        )
        .unwrap();
        let day = &usage.days[0];
        assert_eq!(day.client_app, None);
        assert_eq!(day.counts.requests, 3);
        assert_eq!(day.counts.cached_reported_requests, 0);
        assert_eq!(day.counts.credits, 12.5);
        assert!(usage.months.is_empty());
        assert_eq!(usage.range.start_date, "");
        let period = usage.current_period.unwrap();
        assert_eq!(period.days[0].extra_credits, 0.0);

        let page: UsageRequestsPage = serde_json::from_str(
            r#"{"requests":[{"time":"2026-10-02T10:00:00Z","model":"kimi-k3","source":"api",
                             "client_app":"work","pool":"extra","uncached_input_tokens":10,
                             "cached_input_tokens":null,"output_tokens":2,"credits":0.4}]}"#,
        )
        .unwrap();
        assert_eq!(page.requests[0].cached_input_tokens, None);
        assert_eq!(page.requests[0].client_app.as_deref(), Some("work"));
        assert_eq!(page.next_cursor, None);

        let prices: UsagePrices = serde_json::from_str(r#"{"models":[]}"#).unwrap();
        assert_eq!(prices.credits_per_usd, 1_000.0);
        assert_eq!(prices.unit, "credits_per_million_tokens");
    }
}
