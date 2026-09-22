//! A host server and a remote client over an in-process carrier, with a
//! scripted host behind the server.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use common::{FakeHost, client_config, hello, identity, info, summary};
use maple_agent::agent::{AgentImageUpload, AgentSendMessageRequest, AgentServiceEvent};
use maple_agent::host::{ContextUsage, HostBackend, HostEvent};
use maple_remote::carrier::{Carrier, FrameSink, FrameStream, in_process_pair};
use maple_remote::client::RemoteHostBackend;
use maple_remote::frame::{CONTROL_CHANNEL, Frame, FrameKind};
use maple_remote::rpc::{self, Message, Response};
use maple_remote::server::{HostServer, HostServerConfig, MAX_WATCHED_ROOTS};
use maple_remote::streams::{StreamClose, StreamOpen, UPLOAD_PURPOSE};
use maple_remote::uploads::MAX_UPLOAD_BYTES;
use maple_remote::wire::{EventEnvelope, HostHello, PROTOCOL_VERSION};

/// Start a server on a fresh pair and connect a client through it.
async fn connect(
    host: Arc<FakeHost>,
    config: HostServerConfig,
) -> (
    Arc<RemoteHostBackend>,
    tokio::task::JoinHandle<Result<(), String>>,
) {
    let (client_side, host_side) = in_process_pair(64);
    let server = HostServer::new(host, info(), identity(), config);
    let serving = tokio::spawn(server.serve(host_side));
    let client = RemoteHostBackend::connect(client_side, hello(), client_config())
        .await
        .expect("connect");
    (client, serving)
}

#[tokio::test]
async fn handshake_refuses_a_different_environment_and_protocol() {
    for (protocol, environment) in [(PROTOCOL_VERSION, "Production"), (99, "Development")] {
        let (client_side, host_side) = in_process_pair(8);
        let server = HostServer::new(
            FakeHost::new(0),
            info(),
            identity(),
            HostServerConfig::default(),
        );
        let serving = tokio::spawn(server.serve(host_side));
        let mut hello = hello();
        hello.protocol = protocol;
        hello.pcr_environment = environment.to_string();
        let error = RemoteHostBackend::connect(client_side, hello, client_config())
            .await
            .err()
            .expect("refused");
        assert!(
            error.contains("environment") || error.contains("protocol"),
            "{error}"
        );
        tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("server ends after a refusal")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn snapshots_page_completely_and_calls_round_trip() {
    let host = FakeHost::new(230);
    let (client, serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    assert_eq!(client.id().as_str(), "host-key");
    assert_eq!(client.host_hello().host.name, "workstation");

    let boot = client.bootstrap().await.unwrap();
    assert_eq!(boot.sessions.len(), 2);
    assert_eq!(boot.session_defaults.permission_mode, "auto");
    let latest = boot.latest.unwrap();
    assert_eq!(
        latest.timeline.len(),
        230,
        "every page of the bootstrap snapshot arrives"
    );
    assert_eq!(latest.timeline[229].id, "item-229");

    let detail = client.load_session("s2".to_string()).await.unwrap();
    assert_eq!(detail.timeline.len(), 230);
    assert_eq!(detail.session.id, "s2");
    // The bootstrap loaded s1 once and the load loaded s2 once; paging read
    // the kept snapshots instead of loading again.
    assert_eq!(host.loads.load(Ordering::Relaxed), 1);

    assert_eq!(
        client
            .send_message(AgentSendMessageRequest {
                session_id: "s1".to_string(),
                text: "hi".to_string(),
                model: None,
                context_limit: None,
                mode: None,
                vision_capable: false,
                steer: false,
                queue_id: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap(),
        "run-for-s1"
    );
    assert_eq!(
        client
            .rename_session("s1".to_string(), "Renamed".to_string())
            .await
            .unwrap()
            .title,
        "Renamed"
    );
    assert_eq!(
        client.context_usage("s1".to_string(), None).await.unwrap(),
        Some(ContextUsage {
            tokens: 10,
            limit: 100
        })
    );
    assert_eq!(
        client
            .tool_summaries("s1".to_string())
            .await
            .unwrap()
            .get("item-1")
            .map(String::as_str),
        Some("did a thing")
    );
    assert_eq!(
        client
            .select_project_root("/x".to_string())
            .await
            .unwrap_err(),
        "cannot select /x",
        "host errors keep their message"
    );
    assert_eq!(
        client.suggest_directories("/p/".to_string()).await.unwrap()[0].path,
        "/p/dir"
    );

    client.close().await;
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn events_arrive_in_order_through_the_hub() {
    let host = FakeHost::new(0);
    let (client, _serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    let mut events = client.subscribe();
    host.events.publish(HostEvent::Service(Box::new(
        AgentServiceEvent::SessionCreated(summary("s9")),
    )));
    client.watch_project_root("/p".to_string()).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        first,
        HostEvent::Service(ref event) if matches!(**event, AgentServiceEvent::SessionCreated(ref s) if s.id == "s9")
    ));
    let second = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        second,
        HostEvent::ProjectBranch { ref branch, .. } if branch.as_deref() == Some("main")
    ));
}

#[tokio::test]
async fn attachments_stream_whole_and_a_missing_one_is_an_error() {
    let host = FakeHost::new(0);
    let (client, _serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    let bytes = client
        .read_image_attachment("s1".to_string(), "a1".to_string())
        .await
        .unwrap();
    assert_eq!(bytes, host.attachment);
    let error = client
        .read_image_attachment("s1".to_string(), "missing".to_string())
        .await
        .unwrap_err();
    assert_eq!(error, "no such attachment");
}

/// A raw peer that answers the handshake by hand and then sends whatever
/// frames the test wants, so sequences can be forged.
async fn raw_host(mut carrier: Carrier, frames: Vec<Frame>) {
    let request = carrier.stream.recv().await.expect("hello");
    let Message::Request(request) = rpc::decode(&request.payload).unwrap() else {
        panic!("first message must be the hello request");
    };
    assert_eq!(request.method, "host.hello");
    let answer = HostHello {
        protocol: PROTOCOL_VERSION,
        app_version: "0.1.0".to_string(),
        build: None,
        pcr_environment: "Development".to_string(),
        generation: "gen".to_string(),
        seq: 0,
        features: Default::default(),
        host: info(),
    };
    let response = Response::ok(request.id, serde_json::to_value(answer).unwrap());
    carrier
        .sink
        .send(Frame::control(
            rpc::encode(&Message::Response(response)).unwrap(),
        ))
        .await
        .unwrap();
    for frame in frames {
        carrier.sink.send(frame).await.unwrap();
    }
    // Keep the connection open until the test drops the client.
    while carrier.stream.recv().await.is_some() {}
}

fn event_frame(seq: u64) -> Frame {
    let envelope = EventEnvelope {
        seq,
        event: HostEvent::ProjectBranch {
            project_root: "/p".to_string(),
            branch: Some(format!("b{seq}")),
        },
    };
    Frame::control(
        rpc::encode(&Message::Notification(rpc::Notification::new(
            "event",
            serde_json::to_value(envelope).unwrap(),
        )))
        .unwrap(),
    )
}

#[tokio::test]
async fn a_sequence_gap_publishes_a_resync_before_the_event() {
    let (client_side, host_side) = in_process_pair(16);
    let peer = tokio::spawn(raw_host(
        host_side,
        vec![event_frame(1), event_frame(2), event_frame(4)],
    ));
    let mut config = client_config();
    config.ping_interval = Duration::from_secs(3600);
    let client = RemoteHostBackend::connect(client_side, hello(), config)
        .await
        .unwrap();
    let mut events = client.subscribe();
    let mut seen = Vec::new();
    for _ in 0..4 {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        seen.push(match event {
            HostEvent::ProjectBranch { branch, .. } => branch.unwrap(),
            HostEvent::Resync => "resync".to_string(),
            other => panic!("unexpected {other:?}"),
        });
    }
    assert_eq!(seen, vec!["b1", "b2", "resync", "b4"]);
    client.close().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), peer).await;
}

#[tokio::test]
async fn a_client_that_stops_draining_is_closed_without_blocking_the_host() {
    let host = FakeHost::new(0);
    let (client_side, host_side) = in_process_pair(2);
    let server = HostServer::new(
        Arc::clone(&host) as Arc<dyn HostBackend>,
        info(),
        identity(),
        HostServerConfig {
            max_outbound_bytes: 8 * 1024,
            lease: Duration::from_secs(3600),
            lease_check: Duration::from_millis(20),
            ..Default::default()
        },
    );
    let serving = tokio::spawn(server.serve(host_side));
    // Hand-rolled client: completes the handshake, then never reads.
    let Carrier {
        mut sink,
        mut stream,
    } = client_side;
    let hello_request = rpc::Request::new(1, "host.hello", serde_json::to_value(hello()).unwrap());
    sink.send(Frame::control(
        rpc::encode(&Message::Request(hello_request)).unwrap(),
    ))
    .await
    .unwrap();
    let answer = stream.recv().await.unwrap();
    assert!(matches!(
        rpc::decode(&answer.payload).unwrap(),
        Message::Response(_)
    ));
    // The host keeps emitting; the queue fills past the limit and the
    // server closes this connection while the publisher never waits.
    let publish = tokio::spawn(async move {
        for seq in 0..2000u64 {
            host.events.publish(HostEvent::ProjectBranch {
                project_root: "/p".to_string(),
                branch: Some(format!("{seq}{}", "x".repeat(64))),
            });
        }
    });
    tokio::time::timeout(Duration::from_secs(1), publish)
        .await
        .expect("publishing never blocks")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("the server closes the stalled connection")
        .unwrap()
        .unwrap();
    drop(sink);
}

#[tokio::test]
async fn a_quiet_peer_loses_its_lease_and_a_pinging_client_keeps_it() {
    let host = FakeHost::new(0);
    let config = HostServerConfig {
        lease: Duration::from_millis(150),
        lease_check: Duration::from_millis(20),
        ..Default::default()
    };
    // Pinging client: stays connected past several leases.
    let (client, serving) = connect(Arc::clone(&host), config.clone()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!client.is_closed());
    assert!(!serving.is_finished());
    client.close().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;

    // Quiet peer: handshake only, then silence.
    let (client_side, host_side) = in_process_pair(8);
    let server = HostServer::new(
        Arc::clone(&host) as Arc<dyn HostBackend>,
        info(),
        identity(),
        config,
    );
    let serving = tokio::spawn(server.serve(host_side));
    let Carrier {
        mut sink,
        mut stream,
    } = client_side;
    let hello_request = rpc::Request::new(1, "host.hello", serde_json::to_value(hello()).unwrap());
    sink.send(Frame::control(
        rpc::encode(&Message::Request(hello_request)).unwrap(),
    ))
    .await
    .unwrap();
    stream.recv().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("the lease expires")
        .unwrap()
        .unwrap();
}

/// Complete the handshake by hand on a raw carrier and return its halves.
async fn raw_handshake(client_side: Carrier) -> (Box<dyn FrameSink>, Box<dyn FrameStream>) {
    let Carrier {
        mut sink,
        mut stream,
    } = client_side;
    let hello_request = rpc::Request::new(1, "host.hello", serde_json::to_value(hello()).unwrap());
    sink.send(Frame::control(
        rpc::encode(&Message::Request(hello_request)).unwrap(),
    ))
    .await
    .unwrap();
    let Message::Response(response) = rpc::decode(&stream.recv().await.unwrap().payload).unwrap()
    else {
        panic!("expected the hello answer");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    (sink, stream)
}

/// Send one request on a raw carrier and return its response.
async fn raw_call(
    sink: &mut Box<dyn FrameSink>,
    stream: &mut Box<dyn FrameStream>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Response {
    sink.send(Frame::control(
        rpc::encode(&Message::Request(rpc::Request::new(id, method, params))).unwrap(),
    ))
    .await
    .unwrap();
    loop {
        let frame = stream.recv().await.expect("a response");
        if let Message::Response(response) = rpc::decode(&frame.payload).unwrap()
            && response.id == id
        {
            return response;
        }
    }
}

#[tokio::test]
async fn a_second_hello_is_refused_and_the_hook_runs_once() {
    let hellos = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_hellos = Arc::clone(&hellos);
    let config = HostServerConfig {
        on_client_hello: Some(Arc::new(move |_| {
            hook_hellos.fetch_add(1, Ordering::Relaxed);
        })),
        ..Default::default()
    };
    let (client_side, host_side) = in_process_pair(8);
    let server = HostServer::new(FakeHost::new(0), info(), identity(), config);
    let _serving = tokio::spawn(server.serve(host_side));
    let (mut sink, mut stream) = raw_handshake(client_side).await;
    let again = raw_call(
        &mut sink,
        &mut stream,
        2,
        "host.hello",
        serde_json::to_value(hello()).unwrap(),
    )
    .await;
    let error = again.error.expect("a second hello is refused");
    assert_eq!(error.code, rpc::code::INVALID_REQUEST);
    assert_eq!(error.message, "hello was already sent");
    assert_eq!(hellos.load(Ordering::Relaxed), 1, "the hook ran once");
    // The connection is still usable.
    let ping = raw_call(
        &mut sink,
        &mut stream,
        3,
        "host.ping",
        serde_json::json!({}),
    )
    .await;
    assert!(ping.error.is_none());
}

#[tokio::test]
async fn integration_setup_is_not_a_wire_method() {
    let host = FakeHost::new(0);
    let (client, _serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    let error = client.setup_integration("x".to_string()).await.unwrap_err();
    assert!(error.contains("on the host itself"), "{error}");

    let (client_side, host_side) = in_process_pair(8);
    let server = HostServer::new(host, info(), identity(), HostServerConfig::default());
    let _serving = tokio::spawn(server.serve(host_side));
    let (mut sink, mut stream) = raw_handshake(client_side).await;
    let response = raw_call(
        &mut sink,
        &mut stream,
        2,
        "integration.setup",
        serde_json::json!({"id": "x"}),
    )
    .await;
    let error = response.error.expect("refused");
    assert_eq!(error.code, rpc::code::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn watches_are_capped_and_released_when_the_connection_ends() {
    let host = FakeHost::new(0);
    let (client, serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    client.watch_project_root("/a".to_string()).await.unwrap();
    client.watch_project_root("/a".to_string()).await.unwrap();
    client.watch_project_root("/b".to_string()).await.unwrap();
    client.watch_project_root("/c".to_string()).await.unwrap();
    client.unwatch_project_root("/c".to_string()).await.unwrap();
    client
        .unwatch_project_root("/never".to_string())
        .await
        .unwrap();
    for index in 0..(MAX_WATCHED_ROOTS - 2) {
        client
            .watch_project_root(format!("/many/{index}"))
            .await
            .unwrap();
    }
    let error = client
        .watch_project_root("/one-too-many".to_string())
        .await
        .unwrap_err();
    assert!(error.contains(&MAX_WATCHED_ROOTS.to_string()), "{error}");
    assert_eq!(
        host.unwatched.lock().unwrap().as_slice(),
        ["/c"],
        "an unwatch of a root never watched here is not forwarded"
    );

    client.close().await;
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut unwatched = host.unwatched.lock().unwrap().clone();
    unwatched.sort();
    let mut expected = vec![
        "/a".to_string(),
        "/a".to_string(),
        "/b".to_string(),
        "/c".to_string(),
    ];
    expected.extend((0..(MAX_WATCHED_ROOTS - 2)).map(|index| format!("/many/{index}")));
    expected.sort();
    assert_eq!(unwatched, expected, "every watch was balanced at teardown");
}

#[tokio::test]
async fn requests_before_the_handshake_and_unknown_methods_are_refused() {
    let host = FakeHost::new(0);
    let (client_side, host_side) = in_process_pair(8);
    let server = HostServer::new(host, info(), identity(), HostServerConfig::default());
    let _serving = tokio::spawn(server.serve(host_side));
    let Carrier {
        mut sink,
        mut stream,
    } = client_side;
    let early = rpc::Request::new(7, "session.list", serde_json::json!({}));
    sink.send(Frame::control(
        rpc::encode(&Message::Request(early)).unwrap(),
    ))
    .await
    .unwrap();
    let Message::Response(response) = rpc::decode(&stream.recv().await.unwrap().payload).unwrap()
    else {
        panic!("expected a response");
    };
    assert_eq!(response.error.unwrap().code, rpc::code::NOT_READY);

    let hello_request = rpc::Request::new(1, "host.hello", serde_json::to_value(hello()).unwrap());
    sink.send(Frame::control(
        rpc::encode(&Message::Request(hello_request)).unwrap(),
    ))
    .await
    .unwrap();
    stream.recv().await.unwrap();
    let unknown = rpc::Request::new(8, "session.explode", serde_json::json!({}));
    sink.send(Frame::control(
        rpc::encode(&Message::Request(unknown)).unwrap(),
    ))
    .await
    .unwrap();
    let Message::Response(response) = rpc::decode(&stream.recv().await.unwrap().payload).unwrap()
    else {
        panic!("expected a response");
    };
    assert_eq!(response.error.unwrap().code, rpc::code::METHOD_NOT_FOUND);
}

fn send_request(attachments: Vec<AgentImageUpload>) -> AgentSendMessageRequest {
    AgentSendMessageRequest {
        session_id: "s1".to_string(),
        text: "look".to_string(),
        model: Some("m1".to_string()),
        context_limit: None,
        mode: None,
        vision_capable: true,
        steer: false,
        queue_id: None,
        attachments,
    }
}

fn image(name: &str, len: usize) -> AgentImageUpload {
    use base64::Engine as _;
    let bytes: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
    AgentImageUpload {
        name: name.to_string(),
        data_url: format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        ),
    }
}

#[tokio::test]
async fn a_large_attachment_streams_to_the_host_ahead_of_the_send() {
    let host = FakeHost::new(0);
    let (client, serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    let big = image("screen.png", 6 * 1024 * 1024);
    let small = image("icon.png", 300);
    let run = client
        .send_message(send_request(vec![big.clone(), small.clone()]))
        .await
        .unwrap();
    assert_eq!(run, "run-for-s1");
    {
        let sent = host.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let request = &sent[0];
        assert_eq!(request.text, "look");
        assert_eq!(request.model.as_deref(), Some("m1"));
        assert!(request.vision_capable);
        assert_eq!(request.attachments.len(), 2);
        assert_eq!(request.attachments[0].name, "screen.png");
        assert_eq!(
            request.attachments[0].data_url, big.data_url,
            "the host rebuilt the same data URL from the streamed bytes"
        );
        assert_eq!(request.attachments[1].name, "icon.png");
        assert_eq!(request.attachments[1].data_url, small.data_url);
    }
    // A second send reuses nothing: the uploads were consumed.
    client
        .send_message(send_request(vec![image("again.png", 10)]))
        .await
        .unwrap();
    assert_eq!(
        host.sent.lock().unwrap()[1].attachments[0].name,
        "again.png"
    );
    client.close().await;
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// The next frame on a stream channel, skipping control traffic.
async fn raw_stream_frame(stream: &mut Box<dyn FrameStream>) -> Frame {
    loop {
        let frame = stream.recv().await.expect("a stream frame");
        if frame.channel != CONTROL_CHANNEL {
            return frame;
        }
    }
}

fn open_upload(channel: u16, upload_id: &str, len: u64) -> Frame {
    let open = StreamOpen {
        purpose: UPLOAD_PURPOSE.to_string(),
        request_id: None,
        upload_id: Some(upload_id.to_string()),
        mime: Some("image/png".to_string()),
        len: Some(len),
    };
    Frame {
        channel,
        kind: FrameKind::Open,
        payload: serde_json::to_vec(&open).unwrap().into(),
    }
}

fn refusal(frame: &Frame) -> String {
    assert_eq!(frame.kind, FrameKind::Close);
    serde_json::from_slice::<StreamClose>(&frame.payload)
        .unwrap()
        .error
        .expect("a refusal names its reason")
}

#[tokio::test]
async fn an_oversized_upload_is_refused_and_the_connection_stays_usable() {
    let host = FakeHost::new(0);
    // The client refuses before it opens a stream.
    let (client, _serving) = connect(Arc::clone(&host), HostServerConfig::default()).await;
    let error = client
        .send_message(send_request(vec![image("huge.png", MAX_UPLOAD_BYTES + 1)]))
        .await
        .unwrap_err();
    assert!(error.contains("too large"), "{error}");
    assert!(host.sent.lock().unwrap().is_empty());
    assert_eq!(
        client
            .send_message(send_request(vec![image("ok.png", 64)]))
            .await
            .unwrap(),
        "run-for-s1"
    );

    // The host refuses a stream that declares too much, or sends more
    // than it declared, and keeps answering afterwards.
    let (client_side, host_side) = in_process_pair(8);
    let server = HostServer::new(host, info(), identity(), HostServerConfig::default());
    let _serving = tokio::spawn(server.serve(host_side));
    let (mut sink, mut stream) = raw_handshake(client_side).await;
    sink.send(open_upload(
        1,
        "declared-too-much",
        MAX_UPLOAD_BYTES as u64 + 1,
    ))
    .await
    .unwrap();
    let reply = raw_stream_frame(&mut stream).await;
    assert_eq!(reply.channel, 1);
    assert!(refusal(&reply).contains("byte limit"));

    sink.send(open_upload(3, "lied", 4)).await.unwrap();
    sink.send(Frame {
        channel: 3,
        kind: FrameKind::Data,
        payload: vec![0u8; 5].into(),
    })
    .await
    .unwrap();
    let reply = raw_stream_frame(&mut stream).await;
    assert_eq!(reply.channel, 3);
    assert!(refusal(&reply).contains("declared"));

    let ping = raw_call(
        &mut sink,
        &mut stream,
        2,
        "host.ping",
        serde_json::json!({}),
    )
    .await;
    assert!(ping.error.is_none());
    let send = raw_call(
        &mut sink,
        &mut stream,
        3,
        "run.send",
        serde_json::json!({
            "request": send_request(Vec::new()),
            "uploads": [{"uploadId": "lied", "name": "x.png"}]
        }),
    )
    .await;
    let error = send.error.expect("a refused upload is not usable");
    assert_eq!(error.code, rpc::code::INVALID_PARAMS);
}

#[tokio::test]
async fn a_send_naming_an_unknown_upload_or_inline_images_is_invalid_params() {
    let host = FakeHost::new(0);
    let (client_side, host_side) = in_process_pair(8);
    let server = HostServer::new(
        Arc::clone(&host) as Arc<dyn HostBackend>,
        info(),
        identity(),
        HostServerConfig::default(),
    );
    let _serving = tokio::spawn(server.serve(host_side));
    let (mut sink, mut stream) = raw_handshake(client_side).await;
    let unknown = raw_call(
        &mut sink,
        &mut stream,
        2,
        "run.send",
        serde_json::json!({
            "request": send_request(Vec::new()),
            "uploads": [{"uploadId": "nope", "name": "x.png"}]
        }),
    )
    .await;
    let error = unknown.error.expect("refused");
    assert_eq!(error.code, rpc::code::INVALID_PARAMS);
    assert!(error.message.contains("nope"), "{}", error.message);

    let inline = raw_call(
        &mut sink,
        &mut stream,
        3,
        "run.send",
        serde_json::json!({ "request": send_request(vec![image("x.png", 8)]) }),
    )
    .await;
    let error = inline.error.expect("refused");
    assert_eq!(error.code, rpc::code::INVALID_PARAMS);
    assert!(
        error.message.contains("update the client"),
        "{}",
        error.message
    );
    assert!(host.sent.lock().unwrap().is_empty());

    // A host that does not advertise uploads gets no attachments.
    let (client_side, host_side) = in_process_pair(16);
    let peer = tokio::spawn(raw_host(host_side, Vec::new()));
    let mut config = client_config();
    config.ping_interval = Duration::from_secs(3600);
    let client = RemoteHostBackend::connect(client_side, hello(), config)
        .await
        .unwrap();
    let error = client
        .send_message(send_request(vec![image("x.png", 8)]))
        .await
        .unwrap_err();
    assert!(error.contains("update the host"), "{error}");
    client.close().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), peer).await;
}

#[tokio::test]
async fn uploads_die_with_their_connection() {
    let host = FakeHost::new(0);
    let server = HostServer::new(
        Arc::clone(&host) as Arc<dyn HostBackend>,
        info(),
        identity(),
        HostServerConfig::default(),
    );
    let (client_side, host_side) = in_process_pair(8);
    let serving = tokio::spawn(Arc::clone(&server).serve(host_side));
    let (mut sink, mut stream) = raw_handshake(client_side).await;
    sink.send(open_upload(1, "kept", 3)).await.unwrap();
    sink.send(Frame {
        channel: 1,
        kind: FrameKind::Data,
        payload: b"abc".to_vec().into(),
    })
    .await
    .unwrap();
    sink.send(Frame {
        channel: 1,
        kind: FrameKind::Close,
        payload: Bytes::new(),
    })
    .await
    .unwrap();
    let ack = raw_stream_frame(&mut stream).await;
    assert_eq!((ack.channel, ack.kind), (1, FrameKind::Close));
    assert!(ack.payload.is_empty(), "the host acknowledged the upload");
    sink.close().await;
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let (client_side, host_side) = in_process_pair(8);
    let _serving = tokio::spawn(server.serve(host_side));
    let (mut sink, mut stream) = raw_handshake(client_side).await;
    let send = raw_call(
        &mut sink,
        &mut stream,
        2,
        "run.send",
        serde_json::json!({
            "request": send_request(Vec::new()),
            "uploads": [{"uploadId": "kept", "name": "x.png"}]
        }),
    )
    .await;
    let error = send.error.expect("the upload went with its connection");
    assert_eq!(error.code, rpc::code::INVALID_PARAMS);
    assert!(host.sent.lock().unwrap().is_empty());
}
