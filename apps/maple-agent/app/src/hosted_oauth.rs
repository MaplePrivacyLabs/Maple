//! The one-attempt browser return endpoint. It accepts a session-bound grant,
//! never provider codes, browser credentials, arbitrary URLs, or a general API.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use maple_sdk::{NativeOAuthHandoffGrant, PreparedNativeOAuthHandoff};
use rand::RngCore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 8192;
const CALLBACK_PATH: &str = "/auth/callback";
const RETURN_ERROR: &str = "Browser sign-in could not finish. Start again in Maple Agent.";

pub(crate) struct HandoffListener {
    listener: TcpListener,
    port: u16,
    state: String,
}

impl HandoffListener {
    pub(crate) fn bind() -> Result<Self, String> {
        let socket = TcpSocket::new_v4().map_err(|_| RETURN_ERROR.to_string())?;
        // Do not enable address/port reuse. Windows additionally needs an
        // exclusive bind so another local process cannot take this endpoint.
        #[cfg(windows)]
        exclusive_bind(&socket).map_err(|_| RETURN_ERROR.to_string())?;
        socket
            .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|_| RETURN_ERROR.to_string())?;
        let listener = socket.listen(8).map_err(|_| RETURN_ERROR.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|_| RETURN_ERROR.to_string())?
            .port();
        let mut random = [0_u8; 16];
        rand::rngs::OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| RETURN_ERROR.to_string())?;
        let state = random.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self {
            listener,
            port,
            state,
        })
    }

    pub(crate) fn start_url(
        &self,
        origin: &str,
        provider: &str,
        session_id: &str,
        request_id: &str,
    ) -> String {
        let mut url = reqwest::Url::parse(origin).expect("validated compiled Auth origin");
        url.set_path("/agent/start");
        url.query_pairs_mut()
            .append_pair("transport", "v2")
            .append_pair("provider", provider)
            .append_pair("native_session_id", session_id)
            .append_pair("native_request_id", request_id)
            .append_pair("return_port", &self.port.to_string())
            .append_pair("return_state", &self.state);
        url.into()
    }

    pub(crate) async fn receive(
        &self,
        prepared: &PreparedNativeOAuthHandoff,
    ) -> Result<(NativeOAuthHandoffGrant, CompletionResponse), String> {
        self.receive_matching(|grant| prepared.matches_untrusted_grant_target(grant))
            .await
    }

    async fn receive_matching(
        &self,
        matches_target: impl Fn(&NativeOAuthHandoffGrant) -> bool,
    ) -> Result<(NativeOAuthHandoffGrant, CompletionResponse), String> {
        loop {
            let (mut stream, peer) = self
                .listener
                .accept()
                .await
                .map_err(|_| RETURN_ERROR.to_string())?;
            if peer.ip() != Ipv4Addr::LOCALHOST {
                continue;
            }
            let grant = tokio::time::timeout(READ_TIMEOUT, self.read_grant(&mut stream))
                .await
                .ok()
                .flatten();
            if let Some(grant) = grant.filter(|grant| matches_target(grant)) {
                return Ok((grant, CompletionResponse(stream)));
            }
            // A stray navigation, favicon, malformed request or wrong state
            // must not consume the SDK's one permitted redemption request.
            let _ = tokio::time::timeout(READ_TIMEOUT, stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: 0\r\n\r\n"
            )).await;
        }
    }

    async fn read_grant(&self, stream: &mut TcpStream) -> Option<NativeOAuthHandoffGrant> {
        let mut bytes = Vec::with_capacity(1024);
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 || bytes.len() + read > MAX_REQUEST_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                return parse_request(&bytes, self.port, &self.state);
            }
        }
    }
}

fn parse_request(bytes: &[u8], port: u16, state: &str) -> Option<NativeOAuthHandoffGrant> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return None;
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    let consumed = match request.parse(bytes).ok()? {
        httparse::Status::Complete(consumed) => consumed,
        httparse::Status::Partial => return None,
    };
    if consumed != bytes.len() || request.method != Some("GET") || request.version != Some(1) {
        return None;
    }
    let host = format!("127.0.0.1:{port}");
    let mut hosts = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("host"));
    if hosts.next()?.value != host.as_bytes() || hosts.next().is_some() {
        return None;
    }
    if request.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("transfer-encoding")
            || (header.name.eq_ignore_ascii_case("content-length") && header.value != b"0")
    }) {
        return None;
    }
    let target = request.path?;
    let (path, _) = target.split_once('?')?;
    if path != CALLBACK_PATH || target.contains('#') {
        return None;
    }
    let url = reqwest::Url::parse(&format!("http://{host}{target}")).ok()?;
    let mut received_state = None;
    let mut grant = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "return_state" if received_state.is_none() => received_state = Some(value),
            "handoff_grant" if grant.is_none() => grant = Some(value),
            _ => return None,
        }
    }
    if received_state?.as_ref() != state {
        return None;
    }
    NativeOAuthHandoffGrant::new(grant?.into_owned()).ok()
}

#[cfg(windows)]
fn exclusive_bind(socket: &TcpSocket) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOL_SOCKET, WSAGetLastError, setsockopt,
    };
    let enabled = 1_i32;
    // SAFETY: a live Winsock socket, a valid i32 option pointer and its exact
    // size. setsockopt does not retain the pointer after returning.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as _,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&enabled as *const i32).cast(),
            std::mem::size_of_val(&enabled) as i32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        // SAFETY: reading the current thread's Winsock error has no arguments
        // or retained pointers.
        Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }))
    }
}

pub(crate) struct CompletionResponse(TcpStream);

impl CompletionResponse {
    /// Native publication decides the message. Delivery failure cannot undo a
    /// committed session, and never causes another redemption or redirect.
    pub(crate) fn finish(self, outcome: Result<bool, ()>) {
        tokio::spawn(async move {
            let message = match outcome {
                Ok(true) => "Signed in to Maple Agent. You can close this tab.",
                Ok(false) => {
                    "Signed in to Maple Agent, but the session could not be saved. You will need to sign in again when you restart the app."
                }
                Err(()) => "Sign-in could not finish. Return to Maple Agent and start again.",
            };
            let body = format!(
                "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Maple Agent sign-in</title><script>history.replaceState(null, \"\", \"/auth/callback\");</script><p>{message}</p></html>"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'sha256-TT2/B5X4kutghZnTdCndWuEaf6uS6wbgBxuJIAzSdos='; frame-ancestors 'none'; base-uri 'none'\r\n\r\n{body}",
                body.len()
            );
            let mut stream = self.0;
            let _ = tokio::time::timeout(READ_TIMEOUT, stream.write_all(response.as_bytes())).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "0123456789abcdef0123456789abcdef";
    const GRANT: &str = "e30.e30.c2ln";

    fn request(port: u16, state: &str) -> String {
        format!(
            "GET /auth/callback?handoff_grant={GRANT}&return_state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        )
    }

    #[test]
    fn rejects_wrong_boundary_and_duplicate_or_oversized_input() {
        let valid = request(34567, STATE);
        assert!(parse_request(valid.as_bytes(), 34567, STATE).is_some());
        for invalid in [
            valid.replace("GET ", "POST "),
            valid.replace("/auth/callback?", "/other?"),
            valid.replace("/auth/callback?", "http://127.0.0.1:34567/auth/callback?"),
            valid.replace("Host: 127.0.0.1:34567", "Host: attacker.example"),
            valid.replace("Host: ", "Host: 127.0.0.1:34567\r\nHost: "),
            valid.replace("HTTP/1.1", "HTTP/1.0"),
            valid.replace("?handoff_grant=", "?other=x&handoff_grant="),
            valid.replace("?handoff_grant=", "?handoff_grant=bad&handoff_grant="),
            valid.replace("?handoff_grant=", "?return_state=bad&handoff_grant="),
            valid.replace(STATE, "wrong"),
            valid.replace(GRANT, "not-a-grant"),
            valid.replace("\r\n\r\n", "\r\nContent-Length: 1\r\n\r\nx"),
            valid.replace("\r\n\r\n", "\r\nTransfer-Encoding: chunked\r\n\r\n"),
            valid.replace(" HTTP/", "#fragment HTTP/"),
            valid.replace(GRANT, &"a".repeat(MAX_REQUEST_BYTES)),
        ] {
            assert!(parse_request(invalid.as_bytes(), 34567, STATE).is_none());
        }
        assert!(parse_request(valid.as_bytes(), 34568, STATE).is_none());
    }

    #[tokio::test]
    async fn start_url_advertises_only_the_bound_native_target() {
        let listener = HandoffListener::bind().unwrap();
        let session = "a".repeat(32);
        let request = "b".repeat(32);
        let url = reqwest::Url::parse(&listener.start_url(
            "https://auth-dev.maple.ai",
            "google",
            &session,
            &request,
        ))
        .unwrap();
        assert_eq!(
            url.origin().ascii_serialization(),
            "https://auth-dev.maple.ai"
        );
        assert_eq!(url.path(), "/agent/start");
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.len(), 6);
        assert_eq!(query["transport"], "v2");
        assert_eq!(query["provider"], "google");
        assert_eq!(query["native_session_id"], session);
        assert_eq!(query["native_request_id"], request);
        assert_eq!(query["return_port"], listener.port.to_string());
        assert_eq!(query["return_state"], listener.state);
    }

    #[tokio::test]
    async fn listener_rejects_invalid_requests_then_delivers_once_and_closes() {
        let listener = HandoffListener::bind().unwrap();
        let address = listener.listener.local_addr().unwrap();
        assert_eq!(address.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(listener.state.len(), 32);
        assert!(
            listener
                .state
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert!(std::net::TcpListener::bind(address).is_err());
        let valid = request(listener.port, &listener.state);
        let invalid = request(listener.port, "wrong-state");
        let task = tokio::spawn(async move {
            let (_, response) = listener.receive_matching(|_| true).await.unwrap();
            drop(listener);
            response.finish(Ok(true));
        });
        let mut wrong = TcpStream::connect(address).await.unwrap();
        wrong.write_all(invalid.as_bytes()).await.unwrap();
        let mut rejected = String::new();
        wrong.read_to_string(&mut rejected).await.unwrap();
        assert!(rejected.starts_with("HTTP/1.1 400"));
        let mut browser = TcpStream::connect(address).await.unwrap();
        browser.write_all(valid.as_bytes()).await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        task.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Signed in to Maple Agent"));
        assert!(response.contains("Cache-Control: no-store"));
        assert!(response.contains("history.replaceState"));
        assert!(!response.contains(GRANT));
        assert!(!response.contains("Location:"));
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn wrong_grant_target_does_not_consume_listener() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = HandoffListener::bind().unwrap();
        let address = listener.listener.local_addr().unwrap();
        let valid = request(listener.port, &listener.state);
        let task = tokio::spawn(async move {
            let examined = AtomicUsize::new(0);
            let (_, response) = listener
                .receive_matching(|_| examined.fetch_add(1, Ordering::SeqCst) > 0)
                .await
                .unwrap();
            assert_eq!(examined.load(Ordering::SeqCst), 2);
            response.finish(Ok(false));
        });
        for expected in ["400 Bad Request", "200 OK"] {
            let mut browser = TcpStream::connect(address).await.unwrap();
            browser.write_all(valid.as_bytes()).await.unwrap();
            let mut response = String::new();
            browser.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {expected}")));
            if expected == "200 OK" {
                assert!(response.contains("session could not be saved"));
                assert!(!response.contains(GRANT));
            }
        }
        task.await.unwrap();
    }

    #[tokio::test]
    async fn incomplete_request_times_out_without_consuming_listener() {
        let listener = HandoffListener::bind().unwrap();
        let address = listener.listener.local_addr().unwrap();
        let valid = request(listener.port, &listener.state);
        let task = tokio::spawn(async move {
            let (_, response) = listener.receive_matching(|_| true).await.unwrap();
            response.finish(Err(()));
        });
        let mut stalled = TcpStream::connect(address).await.unwrap();
        stalled.write_all(b"GET /auth").await.unwrap();
        let mut rejected = String::new();
        tokio::time::timeout(
            Duration::from_secs(4),
            stalled.read_to_string(&mut rejected),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(rejected.starts_with("HTTP/1.1 400"));
        let mut browser = TcpStream::connect(address).await.unwrap();
        browser.write_all(valid.as_bytes()).await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("Sign-in could not finish"));
        assert!(!response.contains("Signed in to"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_wait_closes_listener_without_a_callback() {
        let listener = HandoffListener::bind().unwrap();
        let address = listener.listener.local_addr().unwrap();
        let task = tokio::spawn(async move { listener.receive_matching(|_| true).await });
        tokio::task::yield_now().await;
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(TcpStream::connect(address).await.is_err());
    }
}
