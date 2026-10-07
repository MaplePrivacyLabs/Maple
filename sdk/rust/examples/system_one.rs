//! Asks System One three typed questions about a support ticket.
//!
//! Environment: `OPENSECRET_API_URL` (default `http://localhost:3000`) and
//! either `MAPLE_API_KEY`, or `VITE_TEST_EMAIL`, `VITE_TEST_PASSWORD` and
//! `VITE_TEST_CLIENT_ID` to sign in with a password.
//!
//! ```sh
//! MAPLE_API_KEY=... cargo run --example system_one
//! ```

use maple_sdk::{OpenSecretClient, Result, SystemOneAnswer, SystemOneQuestion, SystemOneRequest};
use serde_json::json;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let api_url =
        std::env::var("OPENSECRET_API_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());

    let client = match std::env::var("MAPLE_API_KEY") {
        Ok(api_key) => OpenSecretClient::new_with_api_key(api_url, api_key)?,
        Err(_) => {
            let client = OpenSecretClient::new(api_url)?;
            client.perform_attestation_handshake().await?;
            let email = std::env::var("VITE_TEST_EMAIL").expect("MAPLE_API_KEY or VITE_TEST_EMAIL");
            let password = std::env::var("VITE_TEST_PASSWORD").expect("VITE_TEST_PASSWORD");
            let client_id: Uuid = std::env::var("VITE_TEST_CLIENT_ID")
                .expect("VITE_TEST_CLIENT_ID")
                .parse()
                .expect("VITE_TEST_CLIENT_ID is a UUID");
            client.login(email, password, client_id).await?;
            client
        }
    };

    let request = SystemOneRequest::new(json!({
        "ticket": "Payment went through twice and I need one refunded today."
    }))
    .with_question(
        "is_urgent",
        SystemOneQuestion::noul("Does the customer need help today?"),
    )
    .with_question(
        "intent",
        SystemOneQuestion::choice(
            "What does the customer want?",
            [
                ("refund", "money back"),
                ("question", "information"),
                ("praise", "thanks"),
            ],
        ),
    )
    .with_question(
        "frustration",
        SystemOneQuestion::score("How frustrated is the customer?", ["Low", "Medium", "High"]),
    );

    let response = match client.system_one(request).await {
        Ok(response) => response,
        Err(error) => {
            // Rejections keep the backend's status and `system_one_*` code.
            eprintln!(
                "System One request failed: {error} (status {:?}, code {:?})",
                error.api_status(),
                error.api_error_code()
            );
            return Err(error);
        }
    };

    for (name, answer) in response.answers.iter() {
        match answer {
            SystemOneAnswer::Noul { noul, .. } => println!("{name}: true with p={noul:.3}"),
            SystemOneAnswer::Choice {
                choice,
                probabilities,
                confidence,
                ..
            } => {
                let distribution: Vec<String> = probabilities
                    .iter()
                    .map(|(label, p)| format!("{label}={p:.3}"))
                    .collect();
                println!(
                    "{name}: {choice} (confidence {confidence:.2}; {})",
                    distribution.join(", ")
                );
            }
            SystemOneAnswer::Score { score, legend, .. } => {
                let nearest = legend
                    .get(&score.round().to_string())
                    .map(|level| level.to_string())
                    .unwrap_or_default();
                println!("{name}: level {score:.2} ({nearest})");
            }
        }
    }
    println!(
        "usage: {} input tokens, {} upstream requests",
        response.usage.input_tokens, response.usage.requests
    );
    Ok(())
}
