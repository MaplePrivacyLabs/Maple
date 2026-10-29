//! Compositional pairing regression: real CPace and ConfirmGrant frames over
//! loopback Iroh, two independent durable stores, restart, then the production
//! install-grant capture policy. Enrollment/session admission is a fixture
//! boundary; this is not a signed-payload installation or production-login test.

use super::{
    network, pairing,
    store::{Peer, Store},
    update::capture_install_grant,
    wire,
};
use iroh::{Endpoint, EndpointId, RelayMode, endpoint::Connection};
use maple_agent::store::{Store as Database, StorePaths};
use std::{sync::Arc, time::Duration};

const ACCOUNT: &str = "pairing-lifecycle-fixture";
const API: &str = "http://127.0.0.1:1";
const PROJECT: &str = "pairing-lifecycle-project";
const CHANNEL: &str = "unpackaged";

struct Computer {
    root: tempfile::TempDir,
    database: Arc<Database>,
    policy: Store,
    endpoint: Endpoint,
}

impl Computer {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let database = Arc::new(Database::open(StorePaths::new(root.path())).unwrap());
        database.upsert_account(ACCOUNT, 1).unwrap();
        let mut policy = Store::open(database.clone(), API, PROJECT, CHANNEL, ACCOUNT).unwrap();
        policy.set_enabled(true).unwrap();
        let endpoint = network::bind(policy.key(), RelayMode::Disabled, false, true)
            .await
            .unwrap();
        Self {
            root,
            database,
            policy,
            endpoint,
        }
    }

    async fn reopen(self) -> Self {
        let Self {
            root,
            database,
            policy,
            endpoint,
        } = self;
        let identity = endpoint.id();
        endpoint.close().await;
        drop(policy);
        database.close().unwrap();
        drop(database);
        let database = Arc::new(Database::open(StorePaths::new(root.path())).unwrap());
        assert!(Store::exists(&database, API, PROJECT, CHANNEL, ACCOUNT).unwrap());
        let policy = Store::open(database.clone(), API, PROJECT, CHANNEL, ACCOUNT).unwrap();
        let endpoint = network::bind(policy.key(), RelayMode::Disabled, false, true)
            .await
            .unwrap();
        assert_eq!(
            endpoint.id(),
            identity,
            "the endpoint key must survive restart"
        );
        Self {
            root,
            database,
            policy,
            endpoint,
        }
    }

    async fn close(self) {
        self.endpoint.close().await;
        drop(self.policy);
        self.database.close().unwrap();
    }
}

fn hello(mode: wire::Mode) -> wire::Hello {
    wire::Hello {
        version: wire::VERSION,
        minimum_version: wire::VERSION,
        account: ACCOUNT.into(),
        project: PROJECT.into(),
        deployment: "local".into(),
        channel: CHANNEL.into(),
        mode,
    }
}

fn check_context(hello: &wire::Hello) {
    assert_eq!(hello.account, ACCOUNT);
    assert_eq!(hello.project, PROJECT);
    assert_eq!(hello.deployment, "local");
    assert_eq!(hello.channel, CHANNEL);
}

async fn connections(a: &Endpoint, b: &Endpoint) -> (Connection, Connection) {
    let (client, server) = tokio::join!(
        async { a.connect(b.addr(), wire::ALPN).await.unwrap() },
        async { b.accept().await.unwrap().await.unwrap() },
    );
    assert_eq!(client.remote_id(), b.id());
    assert_eq!(server.remote_id(), a.id());
    (client, server)
}

/// Only enrollment admission is supplied by the fixture. Both CPace proofs and
/// all persistence transitions use the production implementations.
async fn pair(controller: &mut Computer, execution: &mut Computer, lose_ack: bool) -> [u8; 16] {
    let window = *uuid::Uuid::new_v4().as_bytes();
    let controller_id = controller.endpoint.id();
    let execution_id = execution.endpoint.id();
    let controller_key = *controller_id.as_bytes();
    let execution_key = *execution_id.as_bytes();
    let context = pairing::Context {
        controller: &controller_key,
        execution: &execution_key,
        account: ACCOUNT,
        project: PROJECT,
        deployment: "local",
        channel: CHANNEL,
        window: &window,
    };
    let proof =
        pairing::Controller::new(pairing::Code::parse("ABCDEFG2").unwrap(), &context).unwrap();
    let (client, server) = connections(&controller.endpoint, &execution.endpoint).await;
    let outgoing = async {
        let (mut tx, mut rx) = client.open_bi().await.unwrap();
        wire::write(
            &mut tx,
            &hello(wire::Mode::Pair {
                window,
                admission: "fixture-only-enrollment-admission".into(),
            }),
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        controller
            .policy
            .reserve_attempt(execution_id, window)
            .unwrap();
        wire::write(&mut tx, &proof.message(), wire::MAX_PREFACE)
            .await
            .unwrap();
        let answer: Result<(Vec<u8>, Vec<u8>), wire::Refusal> =
            wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        let (message, host_tag) = answer.unwrap();
        let tag = proof.confirm(message, &host_tag).unwrap();
        controller
            .policy
            .remember_host_proof(execution_id, window, "Execution computer".into())
            .unwrap();
        wire::write(&mut tx, &tag.as_slice(), wire::MAX_PREFACE)
            .await
            .unwrap();
        let acknowledgement: Result<Result<[u8; 16], wire::Refusal>, wire::Error> =
            wire::read(&mut rx, wire::MAX_PREFACE).await;
        if lose_ack {
            assert!(acknowledgement.is_err());
        } else {
            let grant = acknowledgement.unwrap().unwrap();
            assert_eq!(grant, window);
            controller.policy.confirm_host(execution_id, grant).unwrap();
        }
    };
    let incoming = async {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let request = wire::read_hello(&mut rx).await.unwrap();
        check_context(&request);
        assert!(
            matches!(request.mode, wire::Mode::Pair { window: received, .. } if received == window)
        );
        let message = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        let proof =
            pairing::Execution::new(pairing::Code::parse("ABCDEFG2").unwrap(), &context, message)
                .unwrap();
        wire::write(
            &mut tx,
            &Ok::<_, wire::Refusal>((proof.message(), proof.confirmation().as_slice())),
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        let tag: Vec<u8> = wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        proof.verify(&tag).unwrap();
        execution
            .policy
            .grant(
                controller_id,
                Peer {
                    name: "Paired controller".into(),
                    grant: window,
                    host_grant: None,
                    controls_this_computer: true,
                    can_connect: false,
                    pending_confirmation: None,
                },
            )
            .unwrap();
        if lose_ack {
            // The host committed its grant; only the acknowledgement is lost.
            tx.reset(91u32.into()).unwrap();
        } else {
            wire::write(&mut tx, &Ok::<_, wire::Refusal>(window), wire::MAX_PREFACE)
                .await
                .unwrap();
            tx.finish().unwrap();
        }
    };
    tokio::join!(outgoing, incoming);
    window
}

async fn confirm_after_restart(controller: &mut Computer, execution: &Computer) {
    let controller_id = controller.endpoint.id();
    let execution_id = execution.endpoint.id();
    let pending = &controller.policy.peers()[&execution_id.to_string()];
    assert!(!pending.can_connect);
    assert!(pending.host_grant.is_none());
    let window = pending
        .pending_confirmation
        .expect("durable proof intent survives lost acknowledgement");
    let (client, server) = connections(&controller.endpoint, &execution.endpoint).await;
    let outgoing = async {
        let (mut tx, mut rx) = client.open_bi().await.unwrap();
        wire::write(
            &mut tx,
            &hello(wire::Mode::ConfirmGrant { window }),
            wire::MAX_PREFACE,
        )
        .await
        .unwrap();
        let confirmed: Result<[u8; 16], wire::Refusal> =
            wire::read(&mut rx, wire::MAX_PREFACE).await.unwrap();
        let confirmed = confirmed.unwrap();
        assert_eq!(confirmed, window);
        controller
            .policy
            .confirm_host(execution_id, confirmed)
            .unwrap();
    };
    let incoming = async {
        let (mut tx, mut rx) = server.accept_bi().await.unwrap();
        let request = wire::read_hello(&mut rx).await.unwrap();
        check_context(&request);
        let wire::Mode::ConfirmGrant { window } = request.mode else {
            panic!("expected confirmation")
        };
        let allowed = execution
            .policy
            .peers()
            .get(&controller_id.to_string())
            .is_some_and(|peer| peer.controls_this_computer && peer.grant == window);
        let confirmed = if allowed {
            Ok(window)
        } else {
            Err(wire::Refusal::terminal(
                "No current grant matches this pairing",
            ))
        };
        wire::write(&mut tx, &confirmed, wire::MAX_PREFACE)
            .await
            .unwrap();
        tx.finish().unwrap();
    };
    tokio::join!(outgoing, incoming);
}

fn outgoing_grant(controller: &Computer, execution: EndpointId) -> [u8; 16] {
    let peer = &controller.policy.peers()[&execution.to_string()];
    assert!(peer.can_connect);
    assert!(peer.pending_confirmation.is_none());
    peer.host_grant
        .expect("pairing must durably retain the confirmed outgoing authority")
}

#[tokio::test]
async fn cpace_confirm_restart_and_mutual_pairing_feed_the_real_install_grant_policy() {
    let exercise = async {
        let mut a = Computer::new().await;
        let mut b = Computer::new().await;
        let b_window = pair(&mut a, &mut b, true).await;
        a = a.reopen().await;
        b = b.reopen().await;
        confirm_after_restart(&mut a, &b).await;
        a = a.reopen().await;
        b = b.reopen().await;
        let a_to_b = outgoing_grant(&a, b.endpoint.id());
        assert_eq!(a_to_b, b_window);
        assert!(capture_install_grant(&b.policy, a.endpoint.id(), a_to_b).is_ok());
        assert!(capture_install_grant(&b.policy, a.endpoint.id(), [0; 16]).is_err());

        // Reversing the roles must not overwrite the already confirmed A→B
        // authority with B→A's different incoming pairing generation.
        let a_window = pair(&mut b, &mut a, false).await;
        assert_ne!(a_window, b_window);
        a = a.reopen().await;
        b = b.reopen().await;
        let a_to_b = outgoing_grant(&a, b.endpoint.id());
        let b_to_a = outgoing_grant(&b, a.endpoint.id());
        assert_eq!(a_to_b, b_window);
        assert_eq!(b_to_a, a_window);
        assert!(capture_install_grant(&b.policy, a.endpoint.id(), a_to_b).is_ok());
        assert!(capture_install_grant(&a.policy, b.endpoint.id(), b_to_a).is_ok());
        assert!(capture_install_grant(&b.policy, a.endpoint.id(), b_to_a).is_err());
        assert!(capture_install_grant(&a.policy, b.endpoint.id(), a_to_b).is_err());
        b.policy.revoke(a.endpoint.id()).unwrap();
        assert!(capture_install_grant(&b.policy, a.endpoint.id(), a_to_b).is_err());
        tokio::join!(a.close(), b.close());
    };
    tokio::time::timeout(Duration::from_secs(30), exercise)
        .await
        .expect("pairing lifecycle timed out");
}
