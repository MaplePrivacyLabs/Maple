mod common;

use base64::{engine::general_purpose, Engine as _};
use futures::StreamExt;
use maple_sdk::{
    ChatCompletionRequest, ChatMessage, EmbeddingInput, EmbeddingRequest, Error, Function,
    OpenSecretClient, Result, Tool,
};
use std::env;
use uuid::Uuid;

static TEST_USER_AUTH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn chat_model() -> String {
    env::var("OPENSECRET_TEST_CHAT_MODEL")
        .or_else(|_| env::var("VITE_TEST_CHAT_MODEL"))
        .unwrap_or_else(|_| "llama3-3-70b".to_string())
}

fn reasoning_model() -> String {
    env::var("OPENSECRET_TEST_REASONING_MODEL")
        .or_else(|_| env::var("VITE_TEST_REASONING_MODEL"))
        .unwrap_or_else(|_| "kimi-k2-5".to_string())
}

fn embedding_model() -> String {
    env::var("OPENSECRET_TEST_EMBEDDING_MODEL")
        .or_else(|_| env::var("VITE_TEST_EMBEDDING_MODEL"))
        .unwrap_or_else(|_| "nomic-embed-text".to_string())
}

fn embedding_dimensions() -> usize {
    for name in [
        "OPENSECRET_TEST_EMBEDDING_DIMENSIONS",
        "VITE_TEST_EMBEDDING_DIMENSIONS",
    ] {
        match env::var(name) {
            Ok(value) => {
                return value
                    .parse::<usize>()
                    .unwrap_or_else(|err| panic!("failed to parse {name}: {err}"));
            }
            Err(env::VarError::NotPresent) => {}
            Err(err) => panic!("failed to read {name}: {err}"),
        }
    }

    768
}

fn is_live_ai_usage_limit(error: &Error) -> bool {
    matches!(
        error,
        Error::Api { status: 403, message } if message.contains("Usage limit reached")
    )
}

async fn setup_authenticated_client() -> Result<OpenSecretClient> {
    // Load .env.local from OpenSecret-SDK directory
    let env_path = std::path::Path::new("../.env.local");
    if env_path.exists() {
        dotenvy::from_path(env_path).ok();
    } else {
        // Fallback to standard .env
        dotenvy::dotenv().ok();
    }

    let base_url = env::var("VITE_OPEN_SECRET_API_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let client = common::new_test_client(base_url)?;
    client.perform_attestation_handshake().await?;

    // Login with test credentials
    let email = env::var("VITE_TEST_EMAIL").expect("VITE_TEST_EMAIL must be set");
    let password = env::var("VITE_TEST_PASSWORD").expect("VITE_TEST_PASSWORD must be set");
    let name = env::var("VITE_TEST_NAME").ok();
    let client_id = env::var("VITE_TEST_CLIENT_ID")
        .expect("VITE_TEST_CLIENT_ID must be set")
        .parse::<Uuid>()
        .expect("Invalid client_id format");

    // Parallel tests share one configured user. Serialize only the login/register
    // bootstrap so they cannot all observe a missing user and race registration.
    let _auth_guard = TEST_USER_AUTH_LOCK.lock().await;
    match client
        .login(email.clone(), password.clone(), client_id)
        .await
    {
        Ok(_) => {}
        Err(_) => {
            // Register the user if login failed
            client.register(email, password, client_id, name).await?;
        }
    }

    Ok(client)
}

#[tokio::test]
async fn test_get_models() {
    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let models = client.get_models().await.expect("Failed to get models");

    // Verify response structure
    assert_eq!(models.object, "list");
    assert!(!models.data.is_empty(), "Should have at least one model");

    // Check that each model has required fields
    for model in &models.data {
        assert!(!model.id.is_empty(), "Model ID should not be empty");
        assert_eq!(model.object, "model");
        // owned_by is optional in the server response
        if let Some(ref owned_by) = model.owned_by {
            assert!(
                !owned_by.is_empty(),
                "Model owner should not be empty if present"
            );
        }
    }

    println!("Found {} models", models.data.len());
    println!("First model: {}", models.data[0].id);
}

#[tokio::test]
async fn test_get_model_catalog() {
    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let catalog = client
        .get_model_catalog()
        .await
        .expect("Failed to get model catalog");

    // Verify response structure
    assert_eq!(catalog.object, "list");
    assert!(
        !catalog.data.is_empty(),
        "Catalog should have at least one model"
    );

    for model in &catalog.data {
        assert!(!model.id.is_empty(), "Catalog model ID should not be empty");
        // Context fields are optional but must be positive when present.
        if let Some(context_window) = model.context_window {
            assert!(context_window > 0, "context_window should be positive");
        }
        if let Some(max_context_tokens) = model.max_context_tokens {
            assert!(
                max_context_tokens > 0,
                "max_context_tokens should be positive"
            );
        }
    }

    for alias in &catalog.aliases {
        assert!(!alias.id.is_empty(), "Alias ID should not be empty");
    }

    // The server always assigns default aliases.
    assert!(
        catalog.defaults.quick.is_some(),
        "Catalog defaults should include a quick model"
    );
    assert!(
        catalog.defaults.powerful.is_some(),
        "Catalog defaults should include a powerful model"
    );

    println!(
        "Found {} catalog models and {} aliases",
        catalog.data.len(),
        catalog.aliases.len()
    );
}

#[tokio::test]
#[ignore = "Server currently only supports streaming completions"]
async fn test_chat_completion_non_streaming() {
    // The server's /v1/chat/completions endpoint only returns SSE streams
    // Non-streaming is not currently implemented on the server side
    // This test is kept for future implementation
}

#[tokio::test]
async fn test_chat_completion_streaming() {
    if !common::live_ai_enabled() {
        eprintln!("Skipping live AI streaming test; set RUN_LIVE_AI=1 to enable it");
        return;
    }

    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let request = ChatCompletionRequest {
        model: chat_model(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: serde_json::json!(r#"please reply with exactly and only the word "echo""#),
            tool_calls: None,
            reasoning_content: None,
        }],
        temperature: Some(0.0),
        max_tokens: Some(10),
        stream: Some(true),
        stream_options: None,
        tools: None,
        tool_choice: None,
    };

    let mut stream = match client.create_chat_completion_stream(request).await {
        Ok(stream) => stream,
        Err(error) if is_live_ai_usage_limit(&error) => {
            eprintln!("Skipping live AI streaming test: usage limit reached");
            return;
        }
        Err(error) => panic!("Failed to create streaming completion: {error:?}"),
    };

    let mut full_response = String::new();
    let mut chunk_count = 0;
    let mut saw_usage = false;
    let mut max_completion_tokens = 0;

    while let Some(result) = stream.next().await {
        let chunk = result.expect("Failed to get chunk");
        chunk_count += 1;

        // Check chunk structure (ChatCompletionChunk is a transparent Value wrapper)
        assert!(chunk.0["id"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(
            chunk.0["object"].as_str().unwrap_or(""),
            "chat.completion.chunk"
        );

        if let Some(choices) = chunk.0["choices"].as_array() {
            if !choices.is_empty() {
                if let Some(s) = choices[0]["delta"]["content"].as_str() {
                    full_response.push_str(s);
                }
            }
        }

        // Check if we got usage in the final chunk
        if chunk.0["usage"].is_object() {
            saw_usage = true;
            assert!(chunk.0["usage"]["prompt_tokens"].as_i64().unwrap_or(0) > 0);
            max_completion_tokens = max_completion_tokens
                .max(chunk.0["usage"]["completion_tokens"].as_i64().unwrap_or(0));
        }
    }

    assert!(chunk_count > 0, "Should have received at least one chunk");
    assert_eq!(full_response.trim().to_lowercase(), "echo");
    assert!(saw_usage, "Should have received usage information");
    assert!(
        max_completion_tokens > 0,
        "Should have received completion token usage"
    );
}

#[tokio::test]
#[ignore = "Known backend regression: kimi provider is not emitting reasoning_content; SDK passthrough is covered by unit tests"]
async fn test_reasoning_content_with_kimi_k2() {
    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let request = ChatCompletionRequest {
        model: reasoning_model(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: serde_json::json!("What is 2+2?"),
            tool_calls: None,
            reasoning_content: None,
        }],
        temperature: Some(0.0),
        max_tokens: Some(100),
        stream: Some(true),
        stream_options: None,
        tools: None,
        tool_choice: None,
    };

    let mut stream = match client.create_chat_completion_stream(request).await {
        Ok(stream) => stream,
        Err(error) if is_live_ai_usage_limit(&error) => {
            eprintln!("Skipping live AI reasoning test: usage limit reached");
            return;
        }
        Err(error) => panic!("Failed to create streaming completion: {error:?}"),
    };

    let mut saw_reasoning_content = false;

    while let Some(result) = stream.next().await {
        let chunk = result.expect("Failed to get chunk");

        if let Some(choices) = chunk.0["choices"].as_array() {
            if !choices.is_empty() && !choices[0]["delta"]["reasoning_content"].is_null() {
                saw_reasoning_content = true;
            }
        }
    }

    assert!(
        saw_reasoning_content,
        "reasoning model should return reasoning_content in the response"
    );
}

#[tokio::test]
async fn test_chat_completion_with_system_message() {
    if !common::live_ai_enabled() {
        eprintln!("Skipping live AI system-message test; set RUN_LIVE_AI=1 to enable it");
        return;
    }

    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let request = ChatCompletionRequest {
        model: chat_model(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: serde_json::json!(
                    "You are a helpful assistant that always responds with exactly one word."
                ),
                tool_calls: None,
                reasoning_content: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: serde_json::json!("What is 2+2? Answer in one word."),
                tool_calls: None,
                reasoning_content: None,
            },
        ],
        temperature: Some(0.0),
        max_tokens: Some(10),
        stream: Some(true), // Server only supports streaming
        stream_options: None,
        tools: None,
        tool_choice: None,
    };

    let mut stream = match client.create_chat_completion_stream(request).await {
        Ok(stream) => stream,
        Err(error) if is_live_ai_usage_limit(&error) => {
            eprintln!("Skipping live AI system-message test: usage limit reached");
            return;
        }
        Err(error) => panic!("Failed to create streaming completion: {error:?}"),
    };

    let mut full_response = String::new();
    while let Some(result) = stream.next().await {
        let chunk = result.expect("Failed to get chunk");
        if let Some(choices) = chunk.0["choices"].as_array() {
            if !choices.is_empty() {
                if let Some(s) = choices[0]["delta"]["content"].as_str() {
                    full_response.push_str(s);
                }
            }
        }
    }

    // Should be a single word like "four" or "4"
    let word_count = full_response.split_whitespace().count();
    assert_eq!(word_count, 1, "Response should be exactly one word");
}

#[tokio::test]
async fn test_delete_conversations() {
    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    // 1. Create a couple of conversations first
    // Note: Currently the SDK doesn't have create_conversation exposed directly on client struct
    // but create_chat_completion implicitly creates or uses conversation if we could access it.
    // However, based on the TS SDK, there's explicit conversation management.
    // The Rust SDK seems to be catching up.
    // For now, we'll just call delete_conversations and ensure it returns success,
    // which covers the API contract even if list is empty.

    // Ideally we would create conversations here, but the Rust SDK client wrapper
    // doesn't seem to expose explicit conversation creation methods yet based on my earlier grep.
    // It only has what I added: delete_conversations.
    // The TS SDK had `createConversation`.

    // Let's verify what methods OpenSecretClient actually has.
    // I only added `delete_conversations`.

    let result = client
        .delete_conversations()
        .await
        .expect("Failed to delete conversations");

    assert_eq!(result.object, "list.deleted");
    assert!(result.deleted);
}

#[tokio::test]
#[ignore = "Paid guest users can access LLMs and models endpoint"]
async fn test_guest_user_cannot_use_ai() {
    // Load .env.local from OpenSecret-SDK directory
    let env_path = std::path::Path::new("../.env.local");
    if env_path.exists() {
        dotenvy::from_path(env_path).ok();
    }

    let base_url = env::var("VITE_OPEN_SECRET_API_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());

    // Use the client_id from env for consistency with other tests
    let client_id = env::var("VITE_TEST_CLIENT_ID")
        .ok()
        .and_then(|id| Uuid::parse_str(&id).ok())
        .unwrap_or_else(Uuid::new_v4);

    let client = common::new_test_client(base_url).expect("Failed to create client");
    client
        .perform_attestation_handshake()
        .await
        .expect("Failed to perform handshake");

    // Register as guest (no email)
    let password = format!("TestGuestPassword_{}", Uuid::new_v4());
    client
        .register_guest(password.to_string(), client_id)
        .await
        .expect("Failed to register guest");

    // Try to get models - should fail
    let models_result = client.get_models().await;
    assert!(
        models_result.is_err(),
        "Guest users should not be able to access models"
    );

    // Try to create completion - should fail
    let request = ChatCompletionRequest {
        model: "some-model".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: serde_json::json!("test"),
            tool_calls: None,
            reasoning_content: None,
        }],
        temperature: None,
        max_tokens: None,
        stream: Some(true), // Server only supports streaming
        stream_options: None,
        tools: None,
        tool_choice: None,
    };

    let completion_result = client.create_chat_completion(request).await;
    assert!(
        completion_result.is_err(),
        "Guest users should not be able to create completions"
    );
}

#[tokio::test]
async fn test_create_embeddings_float_encoding() {
    if !common::live_ai_enabled() {
        eprintln!("Skipping live embeddings test; set RUN_LIVE_AI=1 to enable it");
        return;
    }

    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let request = EmbeddingRequest {
        input: EmbeddingInput::Single("Hello, world!".to_string()),
        model: embedding_model(),
        encoding_format: Some("float".to_string()),
        dimensions: None,
        user: None,
    };

    let response = client
        .create_embeddings(request)
        .await
        .expect("Failed to create embeddings");

    // Verify response structure
    assert_eq!(response.object, "list");
    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].object, "embedding");
    assert_eq!(response.data[0].index, 0);

    let embedding = response.data[0]
        .embedding
        .as_floats()
        .expect("Expected float embedding response");
    assert_eq!(embedding.len(), embedding_dimensions());
    assert!(embedding.iter().all(|value| value.is_finite()));

    // Verify usage
    assert!(response.usage.prompt_tokens > 0);
    assert!(response.usage.total_tokens > 0);

    println!(
        "Embedding created with {} dimensions, {} tokens used",
        embedding.len(),
        response.usage.total_tokens
    );
}

#[tokio::test]
async fn test_create_embeddings_base64_encoding() {
    if !common::live_ai_enabled() {
        eprintln!("Skipping live embeddings test; set RUN_LIVE_AI=1 to enable it");
        return;
    }

    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let request = EmbeddingRequest {
        input: EmbeddingInput::Single("Hello, world!".to_string()),
        model: embedding_model(),
        encoding_format: Some("base64".to_string()),
        dimensions: None,
        user: None,
    };

    let response = client
        .create_embeddings(request)
        .await
        .expect("Failed to create base64 embeddings");

    assert_eq!(response.object, "list");
    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].object, "embedding");
    assert_eq!(response.data[0].index, 0);

    let embedding = response.data[0]
        .embedding
        .as_base64()
        .expect("Expected base64 embedding response");
    let decoded = general_purpose::STANDARD
        .decode(embedding)
        .expect("Embedding should contain valid base64");
    assert_eq!(
        decoded.len(),
        embedding_dimensions() * std::mem::size_of::<f32>()
    );

    assert!(response.usage.prompt_tokens > 0);
    assert!(response.usage.total_tokens > 0);
}

#[tokio::test]
async fn test_create_embeddings_multiple_inputs() {
    if !common::live_ai_enabled() {
        eprintln!("Skipping live embeddings test; set RUN_LIVE_AI=1 to enable it");
        return;
    }

    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let request = EmbeddingRequest {
        input: EmbeddingInput::Multiple(vec![
            "First text to embed".to_string(),
            "Second text to embed".to_string(),
            "Third text to embed".to_string(),
        ]),
        model: embedding_model(),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    let response = client
        .create_embeddings(request)
        .await
        .expect("Failed to create embeddings");

    // Verify response structure
    assert_eq!(response.object, "list");
    assert_eq!(response.data.len(), 3, "Should have 3 embeddings");

    // Check each embedding
    for (i, embedding_data) in response.data.iter().enumerate() {
        assert_eq!(embedding_data.object, "embedding");
        assert_eq!(embedding_data.index as usize, i);
        let embedding = embedding_data
            .embedding
            .as_floats()
            .expect("Expected default float embedding response");
        assert_eq!(embedding.len(), embedding_dimensions());
    }

    // Verify usage accounts for all inputs
    assert!(response.usage.prompt_tokens > 0);

    println!(
        "Created {} embeddings, {} total tokens used",
        response.data.len(),
        response.usage.total_tokens
    );
}

#[tokio::test]
async fn test_embeddings_from_string_conversion() {
    if !common::live_ai_enabled() {
        eprintln!("Skipping live embeddings test; set RUN_LIVE_AI=1 to enable it");
        return;
    }

    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    // Test the From<&str> conversion
    let request = EmbeddingRequest {
        input: "Test string conversion".into(),
        model: embedding_model(),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    let response = client
        .create_embeddings(request)
        .await
        .expect("Failed to create embeddings");

    assert_eq!(response.data.len(), 1);
    assert_eq!(
        response.data[0]
            .embedding
            .as_floats()
            .expect("Expected default float embedding response")
            .len(),
        embedding_dimensions()
    );
}

#[tokio::test]
#[ignore = "Requires live model usage budget for multi-tool streaming"]
async fn test_streaming_multi_tool_calls() {
    let client = setup_authenticated_client()
        .await
        .expect("Failed to setup client");

    let tools = vec![
        Tool {
            tool_type: "function".to_string(),
            function: Function {
                name: "get_weather".to_string(),
                description: Some("Get current weather for a location".to_string()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }),
            },
        },
        Tool {
            tool_type: "function".to_string(),
            function: Function {
                name: "get_time".to_string(),
                description: Some("Get current time for a timezone".to_string()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "timezone": {"type": "string", "description": "IANA timezone"}
                    },
                    "required": ["timezone"]
                }),
            },
        },
    ];

    let request = ChatCompletionRequest {
        model: chat_model(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: serde_json::json!("What is the weather in NYC and what time is it there?"),
            tool_calls: None,
            reasoning_content: None,
        }],
        temperature: Some(0.0),
        max_tokens: Some(512),
        stream: Some(true),
        stream_options: None,
        tools: Some(tools),
        tool_choice: None,
    };

    let mut stream = client
        .create_chat_completion_stream(request)
        .await
        .expect("Failed to create streaming completion");

    let mut chunk_count = 0;
    let mut saw_tool_calls = false;
    let mut tool_names: Vec<String> = Vec::new();

    while let Some(result) = stream.next().await {
        let chunk =
            result.expect("Failed to get chunk - streaming tool_call passthrough may be broken");
        chunk_count += 1;

        if let Some(choices) = chunk.0["choices"].as_array() {
            if !choices.is_empty() {
                // Track tool call names from first delta of each tool
                if let Some(tool_calls) = choices[0]["delta"]["tool_calls"].as_array() {
                    saw_tool_calls = true;
                    for tc in tool_calls {
                        if let Some(name) = tc["function"]["name"].as_str() {
                            if !name.is_empty() {
                                tool_names.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(chunk_count > 0, "Should have received at least one chunk");
    assert!(saw_tool_calls, "Should have seen tool_calls in stream");
    assert!(
        !tool_names.is_empty(),
        "Should have called at least one tool, got: {:?}",
        tool_names
    );
    println!("Tool calls received: {:?}", tool_names);
}
