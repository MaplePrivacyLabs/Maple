mod config;
mod proxy;

pub use config::Config;
pub use maple_sdk::Pcr0Environment;
use proxy::{health_check, proxy_openai_request, ProxyState};

use axum::{
    extract::DefaultBodyLimit,
    http::Method,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    cors::{AllowHeaders, Any, CorsLayer},
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;

const MAX_PROXY_REQUEST_BODY_BYTES: usize = 50 * 1024 * 1024;

/// Create the Axum application with the given configuration
pub fn create_app(config: Config) -> Router {
    let state = Arc::new(ProxyState::new(config.clone()));
    create_app_with_state(config, state)
}

pub(crate) fn create_app_with_state(config: Config, state: Arc<ProxyState>) -> Router {
    let mut app = Router::new()
        // Health check endpoints
        .route("/health", get(health_check))
        .route("/", get(health_check))
        // OpenAI-compatible endpoints
        .route("/v1/models", get(proxy_openai_request))
        .route("/v1/chat/completions", post(proxy_openai_request))
        .route("/v1/embeddings", post(proxy_openai_request))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(DefaultBodyLimit::max(MAX_PROXY_REQUEST_BODY_BYTES))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                        .on_response(DefaultOnResponse::new().level(Level::INFO)),
                ),
        );

    // Add CORS if enabled
    if config.enable_cors {
        app = app.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                // Fetch requires Authorization to be allowed explicitly;
                // a wildcard does not admit the caller key required in CORS mode.
                .allow_headers(AllowHeaders::mirror_request()),
        );
    }

    app
}
