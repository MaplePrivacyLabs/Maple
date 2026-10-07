use maple_proxy::{create_app, Config};
use tracing::{info, Level};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load();

    // Initialize tracing
    let filter = if config.debug {
        EnvFilter::from_default_env().add_directive(Level::DEBUG.into())
    } else {
        EnvFilter::from_default_env().add_directive(Level::INFO.into())
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting Maple Proxy Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    info!("Backend URL: {}", config.backend_url);
    info!("PCR0 environment: {:?}", config.pcr0_environment);
    info!("Binding to: {}", config.socket_addr()?);

    if config.default_api_key.is_some() {
        info!("Default API key configured");
    } else {
        info!("No default API key - clients must provide Authorization header");
    }

    // Build the application
    let app = create_app(config.clone());

    if config.enable_cors {
        info!("CORS enabled for all origins");
    }

    let listener = tokio::net::TcpListener::bind(config.socket_addr()?).await?;

    info!("🚀 Maple Proxy Server started successfully!");
    info!("📋 Available endpoints:");
    info!("   GET  /health              - Health check");
    info!("   GET  /v1/models           - List available models");
    info!("   POST /v1/chat/completions - Create chat completions (streaming & non-streaming)");
    info!("   POST /v1/embeddings       - Create embeddings");
    info!("   POST /v1/systemone        - Typed decisions (System One)");
    info!("");
    info!("💡 Usage:");
    info!(
        "   Set MAPLE_API_KEY environment variable or provide Authorization: Bearer <key> header"
    );
    info!("   OpenAI-compatible clients can use this proxy as their base URL");
    info!("");
    info!("🔗 Example curl:");
    info!("   curl http://{} \\", config.socket_addr()?);
    info!("     -H \"Authorization: Bearer YOUR_MAPLE_API_KEY\" \\");
    info!("     -H \"Content-Type: application/json\" \\");
    info!("     -d '{{\"model\": \"gpt-4\", \"messages\": [{{\"role\": \"user\", \"content\": \"Hello!\"}}]}}'");
    info!("     /v1/chat/completions");

    axum::serve(listener, app).await?;

    Ok(())
}
