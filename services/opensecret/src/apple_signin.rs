use crate::ApiError;
use chrono::Utc;
use jsonwebtoken::{
    decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

// Custom deserializer for boolean values that might come as strings "true"/"false"
fn bool_from_string<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    // Try to deserialize as various types
    struct BoolVisitor;

    impl serde::de::Visitor<'_> for BoolVisitor {
        type Value = Option<bool>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a boolean, string \"true\"/\"false\", or null")
        }

        // Handle actual boolean values
        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(value))
        }

        // Handle string values "true"/"false"
        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            match value.to_lowercase().as_str() {
                "true" => Ok(Some(true)),
                "false" => Ok(Some(false)),
                // Could add more variants like "1"/"0" if needed
                _ => Err(E::custom(format!("Invalid boolean string: {}", value))),
            }
        }

        // Handle null values
        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }
    }

    deserializer.deserialize_any(BoolVisitor)
}

const APPLE_JWKS_URL: &str = "https://appleid.apple.com/auth/keys";
const APPLE_ISSUER: &str = "https://appleid.apple.com";

// Cache of Apple's public keys, with timestamp for refresh logic
pub struct AppleJwksCache {
    keys: HashMap<String, AppleKey>,
    last_updated: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppleKey {
    pub kty: String,
    pub kid: String,
    pub use_type: String,
    pub alg: String,
    pub n: String,
    pub e: String,
}

impl AppleKey {
    pub fn to_decoding_key(&self) -> Result<DecodingKey, ApiError> {
        DecodingKey::from_rsa_components(&self.n, &self.e).map_err(|e| {
            error!("Failed to create decoding key from RSA components: {:?}", e);
            ApiError::InternalServerError
        })
    }
}

// Apple ID token claims
#[derive(Debug, Serialize, Deserialize)]
pub struct AppleIdTokenClaims {
    pub iss: String,           // Issuer (should be https://appleid.apple.com)
    pub sub: String,           // Subject - The unique identifier for the user
    pub aud: String,           // Audience - should match your client_id
    pub exp: i64,              // Expiration time
    pub iat: i64,              // Issued at time
    pub email: Option<String>, // User's email (only on first sign-in)
    #[serde(deserialize_with = "bool_from_string", default)]
    pub email_verified: Option<bool>, // Whether email is verified
    #[serde(
        rename = "is_private_email",
        deserialize_with = "bool_from_string",
        default
    )]
    pub is_private_email: Option<bool>, // Whether it's a private relay email
    pub nonce: Option<String>, // Nonce to prevent replay attacks
    pub nonce_supported: Option<bool>,
    pub real_user_status: Option<i32>, // Apple's assessment of whether this is a real person
    pub auth_time: Option<i64>,        // When the authentication occurred
}

// Manager for Apple JWT verification that handles JWKS caching
pub struct AppleJwtVerifier {
    http_client: Client,
    jwks_cache: Arc<RwLock<AppleJwksCache>>,
}

impl AppleJwtVerifier {
    pub fn new() -> Self {
        Self {
            http_client: crate::http_client::client(),
            jwks_cache: Arc::new(RwLock::new(AppleJwksCache {
                keys: HashMap::new(),
                last_updated: chrono::Utc::now() - chrono::Duration::hours(2), // Force initial fetch
            })),
        }
    }

    // Verify an Apple ID token and return the claims if valid
    pub async fn verify_token(
        &self,
        token: &str,
        audience: &str,
        expected_nonce: Option<&str>,
    ) -> Result<AppleIdTokenClaims, ApiError> {
        self.verify_token_for_audiences(token, &[audience], expected_nonce)
            .await
    }

    pub(crate) async fn verify_token_for_audiences(
        &self,
        token: &str,
        audiences: &[&str],
        expected_nonce: Option<&str>,
    ) -> Result<AppleIdTokenClaims, ApiError> {
        // The caller supplies only the project's configured native audiences.
        // Never select an audience from the unverified token or request.
        if audiences.is_empty() {
            return Err(ApiError::InvalidJwt);
        }
        // Get the kid from the token header
        let header = decode_header(token).map_err(|e| {
            error!("Failed to decode JWT header: {:?}", e);
            ApiError::InvalidJwt
        })?;

        let kid = header.kid.ok_or_else(|| {
            error!("JWT header doesn't contain kid");
            ApiError::InvalidJwt
        })?;

        debug!("Validating Apple JWT with kid: {}", kid);

        // Get the matching public key
        let decoding_key = self.get_key_for_kid(&kid).await?;

        // Configure validation with more comprehensive checks
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(audiences);
        validation.set_issuer(&[APPLE_ISSUER.to_string()]);
        validation.validate_exp = true;
        validation.validate_nbf = true; // Validate "not before" if present
        validation.required_spec_claims = ["iss", "sub", "aud", "exp", "iat"]
            .iter()
            .map(|&s| s.to_string())
            .collect(); // Require these claims
        validation.leeway = 60; // 60 seconds of leeway for clock skew

        // Decode and verify the token
        let token_data =
            jsonwebtoken::decode::<AppleIdTokenClaims>(token, &decoding_key, &validation).map_err(
                |e| {
                    error!("JWT verification failed: {:?}", e);
                    ApiError::InvalidJwt
                },
            )?;

        // Verify nonce if expected_nonce is provided
        if let Some(expected) = expected_nonce {
            match &token_data.claims.nonce {
                Some(token_nonce) => {
                    // OpenSecret retains the raw nonce while the client sends
                    // SHA256(raw nonce) to Apple. Apple returns that supplied
                    // value in the signed token.
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(expected.as_bytes());
                    let hashed_nonce = hex::encode(hasher.finalize());

                    if &hashed_nonce != token_nonce {
                        error!("Nonce mismatch in Apple token");
                        return Err(ApiError::InvalidJwt);
                    }
                }
                None => {
                    error!("Expected nonce but none found in token");
                    return Err(ApiError::InvalidJwt);
                }
            }
        }

        debug!("Apple JWT successfully verified");
        Ok(token_data.claims)
    }

    // Get the appropriate key for the given kid, refreshing from Apple if needed
    async fn get_key_for_kid(&self, kid: &str) -> Result<DecodingKey, ApiError> {
        // First try to get the key without refreshing
        {
            let cache = self.jwks_cache.read().await;
            if let Some(key) = cache.keys.get(kid) {
                if chrono::Utc::now() - cache.last_updated <= chrono::Duration::hours(1) {
                    // Key exists and cache is fresh, return it
                    return key.to_decoding_key();
                }
                // Cache is stale but key exists, continue
            }
            // Either key doesn't exist or cache is stale
        }

        // Key not found or cache is stale, refresh the cache
        debug!("Refreshing Apple JWKS cache");
        self.refresh_jwks().await?;

        // Now try to get the key with a fresh cache
        let cache = self.jwks_cache.read().await;
        let key = cache.keys.get(kid).ok_or_else(|| {
            error!("Key ID {} not found in Apple JWKS even after refresh", kid);
            ApiError::InvalidJwt
        })?;

        key.to_decoding_key()
    }

    // Fetch fresh keys from Apple's JWKS endpoint
    async fn refresh_jwks(&self) -> Result<(), ApiError> {
        #[derive(Deserialize)]
        struct JwksResponse {
            keys: Vec<JwksKey>,
        }

        #[derive(Deserialize)]
        struct JwksKey {
            kty: String,
            kid: String,
            #[serde(rename = "use")]
            use_type: String,
            alg: String,
            n: String,
            e: String,
        }

        info!("Fetching Apple JWKS from {}", APPLE_JWKS_URL);
        let response = self
            .http_client
            .get(APPLE_JWKS_URL)
            .send()
            .await
            .map_err(|e| {
                error!("Failed to fetch Apple JWKS: {:?}", e);
                ApiError::InternalServerError
            })?;

        if !response.status().is_success() {
            error!("Apple JWKS request failed: {}", response.status());
            return Err(ApiError::InternalServerError);
        }

        let jwks: JwksResponse = response.json().await.map_err(|e| {
            error!("Failed to parse Apple JWKS response: {:?}", e);
            ApiError::InternalServerError
        })?;

        let mut cache = self.jwks_cache.write().await;
        cache.keys.clear();

        for key in jwks.keys {
            cache.keys.insert(
                key.kid.clone(),
                AppleKey {
                    kty: key.kty,
                    kid: key.kid,
                    use_type: key.use_type,
                    alg: key.alg,
                    n: key.n,
                    e: key.e,
                },
            );
        }

        cache.last_updated = chrono::Utc::now();
        debug!("Apple JWKS cache refreshed with {} keys", cache.keys.len());
        Ok(())
    }
}

// Validates an Apple ID token using the provided verifier
pub async fn validate_apple_native_token(
    verifier: &AppleJwtVerifier,
    identity_token: &str,
    client_id: &str,
    nonce: Option<&str>,
) -> Result<AppleIdTokenClaims, ApiError> {
    verifier
        .verify_token(identity_token, client_id, nonce)
        .await
}

// Apple client secret JWT claims structure
#[derive(Serialize)]
pub struct AppleClientSecretClaims {
    iss: String, // Team ID
    iat: i64,    // Issued at timestamp
    exp: i64,    // Expiration timestamp
    aud: String, // Audience (always "https://appleid.apple.com")
    sub: String, // Client ID (your Services ID)
}

/// Generates an Apple client secret JWT
///
/// This function creates and signs a JWT that Apple requires for the token
/// exchange step of OAuth2. Apple doesn't use a static client secret like
/// other providers; instead, it requires a JWT signed with your private key.
///
/// * `team_id` - Your Apple Developer Team ID (10-character string)
/// * `key_id` - The Key ID for your private key from Apple Developer portal (10-character string)
/// * `private_key_pem` - The .p8 private key content (PEM format)
/// * `client_id` - Your Apple Services ID (for web flows) or App ID (for native flows)
///
/// Returns a signed JWT string or an error
pub fn generate_apple_client_secret(
    team_id: &str,
    key_id: &str,
    private_key_pem: &str,
    client_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    debug!("Generating Apple client secret JWT");

    // Create JWT header with ES256 algorithm and Key ID
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id.to_owned());

    // Current timestamp for iat claim
    // Backdate by 60 seconds to account for potential clock drift
    let now = Utc::now().timestamp() - 60;

    // Create claims - exp must be less than 6 months (15,777,000 seconds) in the future
    // Using 5 months (150 days) to be safe
    let exp_time = now + 60 * 60 * 24 * 150; // 150 days in seconds

    let claims = AppleClientSecretClaims {
        iss: team_id.to_owned(),
        iat: now,
        exp: exp_time,
        aud: "https://appleid.apple.com".to_owned(),
        sub: client_id.to_owned(),
    };

    // Check if the private key looks like a PEM format
    let has_begin = private_key_pem.contains("-----BEGIN");
    let has_end = private_key_pem.contains("-----END");

    if !has_begin || !has_end {
        error!("Private key doesn't appear to be in PEM format");
        return Err("Invalid private key format - not a valid PEM".into());
    }

    // Create ES256 encoding key from the PEM-formatted private key
    let key = EncodingKey::from_ec_pem(private_key_pem.as_bytes()).map_err(|e| {
        error!("Failed to create encoding key from PEM");
        Box::new(e) as Box<dyn std::error::Error>
    })?;

    // Sign JWT with the private key
    let token = encode(&header, &claims, &key).map_err(|e| {
        error!("Failed to encode JWT");
        Box::new(e) as Box<dyn std::error::Error>
    })?;

    debug!("Generated Apple client secret JWT successfully");
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use openssl::{
        ec::{EcGroup, EcKey},
        nid::Nid,
        pkey::PKey,
        rsa::Rsa,
    };

    fn generate_test_ec_keypair_pems() -> (String, String) {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).expect("EC curve should exist");
        let ec_key = EcKey::generate(&group).expect("EC key should generate");
        let pkey = PKey::from_ec_key(ec_key).expect("EC key should convert to PKey");

        let private_pem = String::from_utf8(
            pkey.private_key_to_pem_pkcs8()
                .expect("private key should serialize"),
        )
        .expect("private key PEM should be UTF-8");

        let public_pem = String::from_utf8(
            pkey.public_key_to_pem()
                .expect("public key should serialize"),
        )
        .expect("public key PEM should be UTF-8");

        (private_pem, public_pem)
    }

    fn generate_test_rsa_key_material() -> (String, String, String) {
        let rsa = Rsa::generate(2048).expect("RSA key should generate");
        let private_pem = String::from_utf8(
            rsa.private_key_to_pem()
                .expect("private key should serialize"),
        )
        .expect("private key PEM should be UTF-8");

        let modulus = URL_SAFE_NO_PAD.encode(rsa.n().to_vec());
        let exponent = URL_SAFE_NO_PAD.encode(rsa.e().to_vec());

        (private_pem, modulus, exponent)
    }

    #[derive(Debug, Deserialize)]
    struct TestAppleClientSecretClaims {
        iss: String,
        iat: i64,
        exp: i64,
        aud: String,
        sub: String,
    }

    #[test]
    fn test_generate_apple_client_secret_round_trip() {
        let (private_key_pem, public_key_pem) = generate_test_ec_keypair_pems();

        let token = generate_apple_client_secret(
            "TEAM123456",
            "KEY1234567",
            &private_key_pem,
            "com.opensecret.test",
        )
        .expect("should generate Apple client secret");

        let header = decode_header(&token).expect("header should decode");
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some("KEY1234567"));

        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_audience(&[APPLE_ISSUER]);

        let decoded = jsonwebtoken::decode::<TestAppleClientSecretClaims>(
            &token,
            &DecodingKey::from_ec_pem(public_key_pem.as_bytes()).expect("public key should parse"),
            &validation,
        )
        .expect("token should decode");

        assert_eq!(decoded.claims.iss, "TEAM123456");
        assert_eq!(decoded.claims.aud, APPLE_ISSUER);
        assert_eq!(decoded.claims.sub, "com.opensecret.test");
        assert!(decoded.claims.exp > decoded.claims.iat);
    }

    #[test]
    fn test_apple_key_rsa_components_decode_rs256_token() {
        let (private_key_pem, modulus, exponent) = generate_test_rsa_key_material();

        let claims = AppleIdTokenClaims {
            iss: APPLE_ISSUER.to_string(),
            sub: "apple-user-id".to_string(),
            aud: "com.opensecret.test".to_string(),
            exp: Utc::now().timestamp() + 3600,
            iat: Utc::now().timestamp(),
            email: Some("user@example.com".to_string()),
            email_verified: Some(true),
            is_private_email: Some(false),
            nonce: None,
            nonce_supported: Some(true),
            real_user_status: Some(1),
            auth_time: None,
        };

        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
                .expect("private key should parse"),
        )
        .expect("token should encode");

        let apple_key = AppleKey {
            kty: "RSA".to_string(),
            kid: "rsa01".to_string(),
            use_type: "sig".to_string(),
            alg: "RS256".to_string(),
            n: modulus,
            e: exponent,
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&["com.opensecret.test"]);
        validation.set_issuer(&[APPLE_ISSUER]);
        validation.required_spec_claims = ["iss", "sub", "aud", "exp", "iat"]
            .iter()
            .map(|claim| claim.to_string())
            .collect();

        let decoded = jsonwebtoken::decode::<AppleIdTokenClaims>(
            &token,
            &apple_key
                .to_decoding_key()
                .expect("components should parse"),
            &validation,
        )
        .expect("token should decode");

        assert_eq!(decoded.claims.sub, "apple-user-id");
        assert_eq!(decoded.claims.aud, "com.opensecret.test");
    }

    #[tokio::test]
    async fn configured_native_audiences_accept_only_exact_signed_matches() {
        let (private_key_pem, modulus, exponent) = generate_test_rsa_key_material();
        let verifier = AppleJwtVerifier::new();
        {
            let mut cache = verifier.jwks_cache.write().await;
            cache.last_updated = Utc::now();
            cache.keys.insert(
                "native-test".to_string(),
                AppleKey {
                    kty: "RSA".to_string(),
                    kid: "native-test".to_string(),
                    use_type: "sig".to_string(),
                    alg: "RS256".to_string(),
                    n: modulus,
                    e: exponent,
                },
            );
        }
        let settings = crate::models::project_settings::AppleOAuthSettings {
            client_id: "com.example.app".to_string(),
            additional_native_client_ids: Some(vec!["com.example.app.dev".to_string()]),
            ..Default::default()
        };
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("native-test".to_string());
        let base_claims = serde_json::json!({
            "iss": APPLE_ISSUER, "sub": "same-team-user",
            "aud": "com.example.app", "iat": Utc::now().timestamp(),
            "exp": Utc::now().timestamp() + 3600,
            "nonce": hex::encode(sha2::Sha256::digest(b"raw-nonce")),
        });
        use sha2::Digest;
        for (audience, accepted) in [
            ("com.example.app", true),
            ("com.example.app.dev", true),
            ("com.example.app.services", false),
            ("com.example.app.dev.evil", false),
            ("com.example.App.dev", false),
            ("com.other.app", false),
        ] {
            let mut claims = base_claims.clone();
            claims["aud"] = serde_json::json!(audience);
            let token = encode(&header, &claims, &encoding_key).unwrap();
            assert_eq!(
                verifier
                    .verify_token_for_audiences(
                        &token,
                        &settings.native_client_ids(),
                        Some("raw-nonce")
                    )
                    .await
                    .is_ok(),
                accepted
            );
            // Existing web verification still accepts only its selected Services ID.
            assert_eq!(
                verifier
                    .verify_token(&token, "com.example.app.services", Some("raw-nonce"))
                    .await
                    .is_ok(),
                audience == "com.example.app.services"
            );
        }
        for (field, value) in [
            ("iss", serde_json::json!("https://attacker.example")),
            ("exp", serde_json::json!(Utc::now().timestamp() - 120)),
            ("nonce", serde_json::json!("wrong-nonce")),
            ("nonce", serde_json::Value::Null),
        ] {
            let mut claims = base_claims.clone();
            claims[field] = value;
            let token = encode(&header, &claims, &encoding_key).unwrap();
            assert!(verifier
                .verify_token_for_audiences(
                    &token,
                    &settings.native_client_ids(),
                    Some("raw-nonce")
                )
                .await
                .is_err());
        }
        let token = encode(&header, &base_claims, &encoding_key).unwrap();
        assert!(verifier
            .verify_token_for_audiences(&token, &[], None)
            .await
            .is_err());
        let (other_private_key, _, _) = generate_test_rsa_key_material();
        let forged = encode(
            &header,
            &base_claims,
            &EncodingKey::from_rsa_pem(other_private_key.as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(verifier
            .verify_token_for_audiences(&forged, &settings.native_client_ids(), Some("raw-nonce"))
            .await
            .is_err());
        let legacy = crate::models::project_settings::AppleOAuthSettings {
            client_id: "com.example.app".to_string(),
            ..Default::default()
        };
        let mut dev_claims = base_claims;
        dev_claims["aud"] = serde_json::json!("com.example.app.dev");
        let dev_token = encode(&header, &dev_claims, &encoding_key).unwrap();
        assert!(verifier
            .verify_token_for_audiences(&dev_token, &legacy.native_client_ids(), Some("raw-nonce"))
            .await
            .is_err());
    }
}
