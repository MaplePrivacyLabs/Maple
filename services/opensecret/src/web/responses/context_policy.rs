//! One server-owned context strategy for the complete logical Response.
//!
//! Flag failures select the existing windowed behavior, rather than denying
//! inference. The decision retains only implementation-owned metadata.

use crate::os_flags::{OsFlagsClient, OsFlagsError};
use crate::AppState;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextStrategy {
    LegacyWindow,
    Compaction,
}

impl ContextStrategy {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::LegacyWindow => "legacy_window",
            Self::Compaction => "compaction",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextDecisionReason {
    Enabled,
    Disabled,
    MissingConfiguration,
    MissingFlag,
    Timeout,
    Unavailable,
}

impl ContextDecisionReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::MissingConfiguration => "missing_configuration",
            Self::MissingFlag => "missing_flag",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContextPolicy {
    pub strategy: ContextStrategy,
    pub reason: ContextDecisionReason,
}

impl ContextPolicy {
    fn legacy_with_reason(reason: ContextDecisionReason) -> Self {
        Self {
            strategy: ContextStrategy::LegacyWindow,
            reason,
        }
    }

    fn from_flag_result(result: Result<Option<bool>, OsFlagsError>) -> Self {
        match result {
            Ok(Some(true)) => Self {
                strategy: ContextStrategy::Compaction,
                reason: ContextDecisionReason::Enabled,
            },
            Ok(Some(false)) => Self::legacy_with_reason(ContextDecisionReason::Disabled),
            Ok(None) => Self::legacy_with_reason(ContextDecisionReason::MissingFlag),
            Err(OsFlagsError::Request(error)) if error.is_timeout() => {
                Self::legacy_with_reason(ContextDecisionReason::Timeout)
            }
            Err(_) => Self::legacy_with_reason(ContextDecisionReason::Unavailable),
        }
    }

    #[cfg(test)]
    pub(super) fn compaction() -> Self {
        Self {
            strategy: ContextStrategy::Compaction,
            reason: ContextDecisionReason::Enabled,
        }
    }

    #[cfg(test)]
    pub(super) fn legacy() -> Self {
        Self::legacy_with_reason(ContextDecisionReason::Disabled)
    }
}

pub(super) async fn resolve(state: &AppState, user: Uuid) -> ContextPolicy {
    resolve_client(state.os_flags(), user).await
}

async fn resolve_client(client: Option<&OsFlagsClient>, user: Uuid) -> ContextPolicy {
    let Some(client) = client else {
        return ContextPolicy::legacy_with_reason(ContextDecisionReason::MissingConfiguration);
    };
    ContextPolicy::from_flag_result(client.get_responses_compaction_flag(user).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os_flags::RESPONSES_COMPACTION_FEATURE_FLAG_KEY;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use tokio::net::TcpListener;

    #[derive(Clone)]
    enum FlagReply {
        Value(Option<bool>),
        Unavailable,
        Slow,
    }

    async fn flags(Path(user): Path<Uuid>, State(reply): State<FlagReply>) -> Response {
        match reply {
            FlagReply::Value(value) => Json(json!({
                "user_uuid": user,
                "flags": value.map(|value| std::collections::HashMap::from([
                    (RESPONSES_COMPACTION_FEATURE_FLAG_KEY, value)
                ])).unwrap_or_default(),
            }))
            .into_response(),
            FlagReply::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            FlagReply::Slow => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        }
    }

    async fn client(reply: FlagReply) -> (OsFlagsClient, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/v1/users/:user/flags", get(flags))
            .with_state(reply);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (
            OsFlagsClient::new(format!("http://{address}"), None).unwrap(),
            server,
        )
    }

    #[tokio::test]
    async fn snapshot_enables_only_an_explicit_successful_true() {
        for (reply, expected) in [
            (FlagReply::Value(Some(true)), ContextPolicy::compaction()),
            (FlagReply::Value(Some(false)), ContextPolicy::legacy()),
            (
                FlagReply::Value(None),
                ContextPolicy::legacy_with_reason(ContextDecisionReason::MissingFlag),
            ),
            (
                FlagReply::Unavailable,
                ContextPolicy::legacy_with_reason(ContextDecisionReason::Unavailable),
            ),
        ] {
            let (client, server) = client(reply).await;
            assert_eq!(
                resolve_client(Some(&client), Uuid::new_v4()).await,
                expected
            );
            server.abort();
        }
        assert_eq!(
            resolve_client(None, Uuid::new_v4()).await,
            ContextPolicy::legacy_with_reason(ContextDecisionReason::MissingConfiguration)
        );
    }

    #[tokio::test]
    async fn timeout_selects_the_old_strategy_with_a_static_reason() {
        let (client, server) = client(FlagReply::Slow).await;
        let policy = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            resolve_client(Some(&client), Uuid::new_v4()),
        )
        .await
        .expect("the feature decision must not wait for the delayed service response");
        assert_eq!(policy.strategy, ContextStrategy::LegacyWindow);
        assert_eq!(policy.reason, ContextDecisionReason::Timeout);
        server.abort();
    }

    #[test]
    fn decision_metadata_never_retains_service_payloads() {
        let policy = ContextPolicy::from_flag_result(Err(OsFlagsError::Service {
            status: StatusCode::FORBIDDEN,
            message: "private service payload must be discarded".into(),
        }));
        assert_eq!(policy.strategy.as_str(), "legacy_window");
        assert_eq!(policy.reason.as_str(), "unavailable");
        assert!(!format!("{policy:?}").contains("private service payload"));
        assert_eq!(ContextPolicy::compaction().strategy.as_str(), "compaction");
        for (reason, label) in [
            (ContextDecisionReason::Enabled, "enabled"),
            (ContextDecisionReason::Disabled, "disabled"),
            (
                ContextDecisionReason::MissingConfiguration,
                "missing_configuration",
            ),
            (ContextDecisionReason::MissingFlag, "missing_flag"),
            (ContextDecisionReason::Timeout, "timeout"),
            (ContextDecisionReason::Unavailable, "unavailable"),
        ] {
            assert_eq!(reason.as_str(), label);
        }
    }
}
