//! Real HTTP v2 smoke client, invoked only by scripts/test-recovery-flow.sh.
//! Local mock attestation and a disposable email-proof fixture are intentional;
//! this is not an SDK or Nitro attestation verifier.
use crate::{
    db::{setup_db, DBConnection},
    models::{org_projects::OrgProject, password_reset::PasswordResetRequest, users::User},
    seed_wrapping::password_reset_code_mac,
    transport_v2::{
        crypto::{
            attestation_user_data, derive_client_session, HandshakeTranscript, SessionSecrets,
        },
        envelope::{Credential, CredentialKind, LogicalHeader, RequestEnvelope, RequestId},
        framing::ResponseRecord,
    },
};
use base64::{engine::general_purpose::STANDARD, Engine};
use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
use rand_core::OsRng;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
use x25519_dalek::{EphemeralSecret, PublicKey};

struct Client {
    http: reqwest::Client,
    url: String,
    secrets: SessionSecrets,
    routing: String,
}

impl Client {
    async fn connect(url: String) -> Self {
        let parsed = reqwest::Url::parse(&url).expect("local URL required");
        assert_eq!(parsed.scheme(), "http");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret).to_bytes();
        let challenge = crate::encrypt::generate_random::<32>();
        let routing = STANDARD.encode(challenge);
        let response = http.post(format!("{url}/v2/session"))
            .header("x-opensecret-routing-key", &routing)
            .json(&json!({"version":2,"challenge":routing,"client_public_key":STANDARD.encode(public)}))
            .send().await.expect("session HTTP request");
        assert_eq!(response.status().as_u16(), 200, "session status");
        let response: Value = response.json().await.unwrap();
        let cose: serde_cbor::Value = serde_cbor::from_slice(
            &STANDARD
                .decode(response["attestation_document"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        let serde_cbor::Value::Array(cose) = cose else {
            panic!("local mock COSE array required")
        };
        let serde_cbor::Value::Bytes(payload) = &cose[2] else {
            panic!("COSE payload required")
        };
        let serde_cbor::Value::Map(document) = serde_cbor::from_slice(payload).unwrap() else {
            panic!("attestation map required")
        };
        let field = |name: &str| match document.get(&serde_cbor::Value::Text(name.into())).unwrap()
        {
            serde_cbor::Value::Bytes(bytes) => bytes.clone(),
            _ => panic!("attestation bytes required"),
        };
        assert!(field("nonce") == challenge, "challenge binding");
        assert!(
            field("user_data") == attestation_user_data(&public),
            "client binding"
        );
        let transcript =
            HandshakeTranscript::new(challenge, public, field("public_key").try_into().unwrap());
        let secrets = derive_client_session(secret, &transcript).unwrap();
        assert!(
            response["session_id"].as_str().unwrap() == secrets.session_id().to_string(),
            "session binding"
        );
        Self {
            http,
            url,
            secrets,
            routing,
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
        expected: u16,
    ) -> Value {
        self.raw(
            method,
            path,
            body.map(|v| v.to_string().into_bytes()),
            token.map(|t| Credential::new(CredentialKind::Bearer, t.into()).unwrap()),
            expected,
        )
        .await
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        credential: Option<Credential>,
        expected: u16,
    ) -> Value {
        let id = RequestId::random();
        let envelope = RequestEnvelope::new(
            id,
            credential,
            None,
            method.into(),
            path.into(),
            vec![LogicalHeader::new("content-type".into(), "application/json".into()).unwrap()],
            body,
        )
        .unwrap();
        let sealed = self
            .secrets
            .encrypt_request(id, &envelope.encode().unwrap())
            .unwrap();
        let response = self
            .http
            .post(format!("{}/v2/request", self.url))
            .header("content-type", "application/octet-stream")
            .header("x-session-id", self.secrets.session_id().to_string())
            .header("x-opensecret-routing-key", &self.routing)
            .body(sealed)
            .send()
            .await
            .expect("encrypted HTTP request");
        assert_eq!(
            response.status().as_u16(),
            200,
            "outer carrier status for {method} {path}"
        );
        assert_eq!(
            response.headers()["content-type"],
            "application/octet-stream"
        );
        let bytes = response.bytes().await.unwrap();
        assert!(bytes.len() < 1024 * 1024, "bounded smoke response");
        let mut remaining = bytes.as_ref();
        let mut status = None;
        let mut ended = false;
        let mut plaintext = Vec::new();
        let mut sequence = 0;
        while !remaining.is_empty() {
            assert!(!ended && remaining.len() >= 4, "framing/terminal order");
            let length = u32::from_be_bytes(remaining[..4].try_into().unwrap()) as usize;
            assert!(length <= remaining.len() - 4, "complete frame");
            let opened = self
                .secrets
                .decrypt_response(id, sequence, &remaining[4..4 + length])
                .unwrap();
            match ResponseRecord::decode(&opened).unwrap() {
                ResponseRecord::Start(head) => {
                    assert!(status.is_none() && sequence == 0);
                    status = Some(head.status());
                }
                ResponseRecord::Chunk(chunk) => {
                    assert!(status.is_some());
                    plaintext.extend_from_slice(&chunk);
                }
                ResponseRecord::End => {
                    assert!(status.is_some());
                    ended = true;
                }
                ResponseRecord::Error { .. } => {
                    panic!("unexpected authenticated transport failure")
                }
            }
            sequence += 1;
            remaining = &remaining[4 + length..];
        }
        assert!(ended, "authenticated terminal record required");
        assert_eq!(status, Some(expected), "logical status for {method} {path}");
        let value = serde_json::from_slice(&plaintext).unwrap_or(Value::Null);
        if expected == 400 {
            assert!(
                value == json!({"status":400,"message":"Bad Request"}),
                "sanitized error for {path}"
            );
        }
        value
    }
}

struct Fixture {
    db: Arc<dyn DBConnection + Send + Sync>,
    project: OrgProject,
    user: User,
    root: Vec<u8>,
    secrets: Vec<String>,
}

impl Fixture {
    // Exercise the real reset-request endpoint first. Since local runs have no
    // email credentials, replace only this test user's new reset-code MAC with
    // a known random fixture, preserving the server-created row and secret hash.
    async fn proof(&mut self, client: &Client) -> Value {
        use crate::models::schema::password_reset_requests as r;
        let secret = Uuid::new_v4().to_string();
        let code = Uuid::new_v4().simple().to_string();
        let hash = crate::generate_reset_hash(secret.clone());
        client
            .call(
                "POST",
                "/password-reset/request",
                Some(json!({
                    "email":self.user.email,"hashed_secret":hash,"client_id":self.project.client_id
                })),
                None,
                200,
            )
            .await;
        let conn = &mut self.db.get_pool().get().unwrap();
        let row = r::table
            .filter(r::user_id.eq(self.user.uuid))
            .filter(r::hashed_secret.eq(&hash))
            .first::<PasswordResetRequest>(conn)
            .expect("request endpoint must create the reset row");
        let mac =
            password_reset_code_mac(&self.root, self.project.id, self.user.uuid, &code).unwrap();
        assert_eq!(
            diesel::update(
                r::table
                    .filter(r::id.eq(row.id))
                    .filter(r::user_id.eq(self.user.uuid))
            )
            .set(r::encrypted_code.eq(mac.to_vec()))
            .execute(conn)
            .unwrap(),
            1
        );
        self.secrets.extend([secret.clone(), code.clone()]);
        json!({"email":self.user.email,"alphanumeric_code":code,"plaintext_secret":secret,"client_id":self.project.client_id})
    }

    fn remember(&mut self, value: &Value) {
        for key in ["recovery_code", "access_token", "refresh_token", "mnemonic"] {
            if let Some(secret) = value[key].as_str() {
                self.secrets.push(secret.into());
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.db.delete_user(&self.user);
    }
}

fn complete(proof: &Value, password: &str, code: &str) -> Value {
    json!({"proof":proof,"new_password":password,"mode":{"preserve":{"recovery_code":code}}})
}

#[tokio::test]
#[ignore = "explicit loopback HTTP smoke; run scripts/test-recovery-flow.sh"]
async fn encrypted_account_flow() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let url = std::env::var("RECOVERY_SMOKE_URL").expect("use the smoke script");
    let database_url =
        std::env::var("RECOVERY_SMOKE_DATABASE_URL").expect("disposable database required");
    let database = reqwest::Url::parse(&database_url).unwrap();
    assert_eq!(database.host_str(), Some("127.0.0.1"));
    assert!(database.path().starts_with("/opensecret_recovery_"));
    let root = hex::decode(std::env::var("ENCLAVE_SECRET_MOCK").unwrap()).unwrap();
    let db = setup_db(database_url);
    let conn = &mut db.get_pool().get().unwrap();
    use crate::models::schema::org_projects;
    let project = org_projects::table
        .filter(org_projects::status.eq("active"))
        .order(org_projects::id.asc())
        .first::<OrgProject>(conn)
        .unwrap();
    let client = Client::connect(url).await;
    let email = format!("recovery-smoke-{}@local.test", Uuid::new_v4());
    let password = format!("Recovery-smoke-{}", Uuid::new_v4());
    let next_password = format!("Recovery-next-{}", Uuid::new_v4());
    let auth = client
        .call(
            "POST",
            "/register",
            Some(json!({"email":email,"password":password,
        "client_id":project.client_id,"name":"Recovery smoke"})),
            None,
            200,
        )
        .await;
    let user = db.get_user_by_email(email.clone(), project.id).unwrap();
    let mut fixture = Fixture {
        db,
        project,
        user,
        root,
        secrets: vec![password.clone(), next_password.clone()],
    };
    fixture.remember(&auth);
    let old_token = auth["access_token"].as_str().unwrap();
    let old_refresh = auth["refresh_token"].as_str().unwrap();
    let base = "/protected/recovery-code";
    let options = "/password-reset/v2/options";
    let completion = "/password-reset/v2/complete";
    let status = client.call("GET", base, None, Some(old_token), 200).await;
    assert!(status == json!({"enrolled":false,"enrolled_at":null}));
    for (method, path, body) in [
        ("GET", base, None),
        (
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({"current_password":password})),
        ),
        (
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({"current_password":password})),
        ),
        ("DELETE", base, Some(json!({"current_password":password}))),
    ] {
        client.call(method, path, body.clone(), None, 401).await;
        client
            .raw(
                method,
                path,
                body.clone().map(|b| b.to_string().into_bytes()),
                Some(
                    Credential::new(CredentialKind::ApiKey, "test-not-a-user-jwt".into()).unwrap(),
                ),
                401,
            )
            .await;
        if method != "GET" {
            client
                .call(
                    method,
                    path,
                    Some(json!({"current_password":"wrong"})),
                    Some(old_token),
                    401,
                )
                .await;
            for invalid in [
                json!({}),
                json!({"current_password":null}),
                json!({"current_password":42}),
            ] {
                client
                    .call(method, path, Some(invalid), Some(old_token), 400)
                    .await;
            }
        }
    }
    client
        .call(
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({"current_password":password})),
            Some(old_token),
            400,
        )
        .await;
    let enroll = client
        .call(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({"current_password":password})),
            Some(old_token),
            200,
        )
        .await;
    fixture.remember(&enroll);
    let code = enroll["recovery_code"].as_str().unwrap();
    assert!(crate::recovery_code::RecoveryCode::parse(code).is_ok());
    client
        .call(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({"current_password":password})),
            Some(old_token),
            409,
        )
        .await;
    assert_eq!(
        client.call("GET", base, None, Some(old_token), 200).await["enrolled"],
        true
    );
    let data = format!("private-recovery-marker-{}", Uuid::new_v4());
    fixture.secrets.push(data.clone());
    client
        .call(
            "PUT",
            "/protected/kv/recovery-smoke",
            Some(json!(data)),
            Some(old_token),
            200,
        )
        .await;
    let private = client
        .call("GET", "/protected/private_key", None, Some(old_token), 200)
        .await;
    fixture.remember(&private);
    println!("PASS registration, JWT/API-key boundaries, password step-up, enrollment and encrypted storage");

    let proof = fixture.proof(&client).await;
    let expired = fixture.proof(&client).await;
    use crate::models::schema::password_reset_requests as reset_rows;
    diesel::update(
        reset_rows::table
            .filter(reset_rows::user_id.eq(fixture.user.uuid))
            .filter(reset_rows::hashed_secret.eq(crate::generate_reset_hash(
                expired["plaintext_secret"].as_str().unwrap().into(),
            ))),
    )
    .set(reset_rows::expiration_time.eq(chrono::Utc::now() - chrono::Duration::seconds(1)))
    .execute(conn)
    .unwrap();
    client
        .call("POST", options, Some(json!({"proof":expired})), None, 400)
        .await;
    client
        .call(
            "POST",
            completion,
            Some(complete(&expired, &next_password, code)),
            None,
            400,
        )
        .await;
    for raw in [b"{".to_vec(), b"null".to_vec(), b"[]".to_vec(),
        format!("{{\"proof\":{},\"new_password\":\"one\",\"new_password\":\"two\",\"mode\":{{\"destructive\":{{\"acknowledge_data_loss\":true}}}}}}", proof).into_bytes()] {
        client.raw("POST",completion,Some(raw),None,400).await;
    }
    for _ in 0..2 {
        assert!(
            client
                .call("POST", options, Some(json!({"proof":proof})), None, 200)
                .await
                == json!({"recovery_enrolled":true,"destructive_reset_available":true})
        );
    }
    for key in [
        "email",
        "alphanumeric_code",
        "plaintext_secret",
        "client_id",
    ] {
        let mut bad = proof.clone();
        bad[key] = json!(if key == "client_id" {
            Uuid::new_v4().to_string()
        } else {
            "wrong@local.test".into()
        });
        client
            .call("POST", options, Some(json!({"proof":bad})), None, 400)
            .await;
        client
            .call(
                "POST",
                completion,
                Some(complete(&bad, &next_password, code)),
                None,
                400,
            )
            .await;
    }
    for invalid in [
        "".into(),
        "MPLRC1-bad".into(),
        format!("MPLRC1{}é{}", "0".repeat(51), "0".repeat(7)),
        " ".repeat(257),
        format!(
            "{}{}",
            &code[..code.len() - 1],
            if code.ends_with('0') { "1" } else { "0" }
        ),
    ] {
        client
            .call(
                "POST",
                completion,
                Some(complete(&proof, &next_password, &invalid)),
                None,
                400,
            )
            .await;
        client
            .call("POST", options, Some(json!({"proof":proof})), None, 200)
            .await;
    }
    for mode in [
        json!(null),
        json!({"preserve":{}}),
        json!({"destructive":{"acknowledge_data_loss":false}}),
        json!({"destructive":{"acknowledge_data_loss":"true"}}),
        json!({"destructive":{"acknowledge_data_loss":true,"recovery_code":code}}),
        json!({"preserve":{"recovery_code":code},"destructive":{"acknowledge_data_loss":true}}),
    ] {
        client
            .call(
                "POST",
                completion,
                Some(json!({"proof":proof,"new_password":next_password,"mode":mode})),
                None,
                400,
            )
            .await;
    }
    for value in [json!(code), json!(""), json!(null), json!(42), json!({})] {
        let mut legacy = proof.clone();
        legacy["new_password"] = json!(next_password);
        legacy["recovery_code"] = value;
        client
            .call("POST", "/password-reset/confirm", Some(legacy), None, 400)
            .await;
    }
    let wrap = fixture
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .unwrap()
        .unwrap();
    let sibling = fixture.proof(&client).await;
    let recovered = client
        .call(
            "POST",
            completion,
            Some(complete(
                &proof,
                &next_password,
                &code.to_ascii_lowercase().replace('-', " "),
            )),
            None,
            200,
        )
        .await;
    fixture.remember(&recovered);
    let token = recovered["access_token"].as_str().unwrap();
    for used in [&proof, &sibling] {
        client
            .call("POST", options, Some(json!({"proof":used})), None, 400)
            .await;
        client
            .call(
                "POST",
                completion,
                Some(complete(used, &next_password, code)),
                None,
                400,
            )
            .await;
    }
    client.call("GET", base, None, Some(old_token), 401).await;
    client
        .raw(
            "POST",
            "/refresh",
            None,
            Some(Credential::new(CredentialKind::Resumption, old_refresh.into()).unwrap()),
            401,
        )
        .await;
    let refreshed = client
        .raw(
            "POST",
            "/refresh",
            None,
            Some(
                Credential::new(
                    CredentialKind::Resumption,
                    recovered["refresh_token"].as_str().unwrap().into(),
                )
                .unwrap(),
            ),
            200,
        )
        .await;
    fixture.remember(&refreshed);
    client
        .call(
            "POST",
            "/login",
            Some(json!({"email":email,"password":password,"client_id":fixture.project.client_id})),
            None,
            401,
        )
        .await;
    let login = client.call("POST","/login",Some(json!({"email":email,"password":next_password,"client_id":fixture.project.client_id})),None,200).await;
    fixture.remember(&login);
    assert!(
        client
            .call("GET", "/protected/private_key", None, Some(token), 200)
            .await
            == private,
        "seed identity preserved"
    );
    assert!(
        client
            .call(
                "GET",
                "/protected/kv/recovery-smoke",
                None,
                Some(token),
                200
            )
            .await
            == json!(data),
        "encrypted data readable"
    );
    let after = fixture
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .unwrap()
        .unwrap();
    assert!(
        wrap.id == after.id && wrap.seed_enc == after.seed_enc,
        "recovery wrap unchanged"
    );
    println!("PASS proof permutations, malformed/checksum/Unicode codes, explicit modes, legacy guard, preserving reset and token lifecycle");

    // A new HTTP session proves re-entry and that no client-local continuation is required.
    let client = Client::connect(client.url.clone()).await;
    let proof = fixture.proof(&client).await;
    let reused = client
        .call(
            "POST",
            completion,
            Some(complete(&proof, &next_password, code)),
            None,
            200,
        )
        .await;
    fixture.remember(&reused);
    let token = reused["access_token"].as_str().unwrap();
    let rotated = client
        .call(
            "POST",
            "/protected/recovery-code/rotate",
            Some(json!({"current_password":next_password})),
            Some(token),
            200,
        )
        .await;
    fixture.remember(&rotated);
    let new_code = rotated["recovery_code"].as_str().unwrap();
    assert!(new_code != code);
    let wrong_proof = fixture.proof(&client).await;
    let good_proof = fixture.proof(&client).await;
    client
        .call(
            "POST",
            completion,
            Some(complete(&wrong_proof, &next_password, code)),
            None,
            400,
        )
        .await;
    client
        .call(
            "POST",
            options,
            Some(json!({"proof":wrong_proof})),
            None,
            400,
        )
        .await;
    client
        .call(
            "POST",
            options,
            Some(json!({"proof":good_proof})),
            None,
            200,
        )
        .await;
    let reset = client
        .call(
            "POST",
            completion,
            Some(complete(&good_proof, &next_password, new_code)),
            None,
            200,
        )
        .await;
    fixture.remember(&reset);
    let token = reset["access_token"].as_str().unwrap();
    for _ in 0..2 {
        client
            .call(
                "DELETE",
                base,
                Some(json!({"current_password":next_password})),
                Some(token),
                200,
            )
            .await;
    }
    assert_eq!(
        client.call("GET", base, None, Some(token), 200).await["enrolled"],
        false
    );
    let proof = fixture.proof(&client).await;
    client
        .call(
            "POST",
            completion,
            Some(complete(&proof, &next_password, new_code)),
            None,
            400,
        )
        .await;
    assert_eq!(
        client
            .call("POST", options, Some(json!({"proof":proof})), None, 200)
            .await["recovery_enrolled"],
        false
    );
    // Re-enroll, then choose explicit destruction: code and encrypted data must disappear.
    let reenrolled = client
        .call(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({"current_password":next_password})),
            Some(token),
            200,
        )
        .await;
    fixture.remember(&reenrolled);
    let destroyed = client
        .call(
            "POST",
            completion,
            Some(json!({"proof":proof,"new_password":password,
        "mode":{"destructive":{"acknowledge_data_loss":true}}})),
            None,
            200,
        )
        .await;
    fixture.remember(&destroyed);
    let token = destroyed["access_token"].as_str().unwrap();
    assert_eq!(
        client.call("GET", base, None, Some(token), 200).await["enrolled"],
        false
    );
    assert!(
        client
            .call("GET", "/protected/private_key", None, Some(token), 200)
            .await
            != private
    );
    assert!(fixture
        .db
        .get_recovery_wrap(fixture.user.uuid)
        .unwrap()
        .is_none());
    use crate::models::schema::user_kv;
    assert_eq!(
        user_kv::table
            .filter(user_kv::user_id.eq(fixture.user.uuid))
            .count()
            .get_result::<i64>(conn)
            .unwrap(),
        0
    );
    client.call("POST",completion,Some(json!({"proof":proof,"new_password":password,"mode":{"destructive":{"acknowledge_data_loss":true}}})),None,400).await;
    println!("PASS code reuse, rotation, selected-proof consumption, disable/re-enroll, destructive cleanup and replay");

    // The old-client one-shot payload still works without recovery material,
    // even for an enrolled account, and remains deliberately destructive.
    let legacy_enroll = client
        .call(
            "POST",
            "/protected/recovery-code/enroll",
            Some(json!({"current_password":password})),
            Some(token),
            200,
        )
        .await;
    fixture.remember(&legacy_enroll);
    let mut legacy = fixture.proof(&client).await;
    legacy["new_password"] = json!(next_password);
    client
        .call("POST", "/password-reset/confirm", Some(legacy), None, 200)
        .await;
    let login = client.call("POST","/login",Some(json!({"email":email,"password":next_password,"client_id":fixture.project.client_id})),None,200).await;
    fixture.remember(&login);
    assert_eq!(
        client
            .call("GET", base, None, login["access_token"].as_str(), 200)
            .await["enrolled"],
        false
    );
    println!("PASS legacy destructive reset compatibility for enrolled users");

    if let Ok(path) = std::env::var("RECOVERY_SMOKE_LOG") {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let log = std::fs::read_to_string(path).expect("owned backend log");
        for secret in &fixture.secrets {
            assert!(
                secret.len() >= 8 && !log.contains(secret),
                "sensitive value appeared in backend log"
            );
        }
        assert!(!log.contains("panicked at"), "backend panic in smoke run");
        println!("PASS runtime log scan for generated credentials, codes, tokens and private data");
    }
}
