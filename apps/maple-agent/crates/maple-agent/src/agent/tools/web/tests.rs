use std::sync::Mutex;

use maple_sdk::{WebExtractPage, WebExtractResponse, WebSearchResponse};
use pi_agent_core::{ToolInvocation, ToolUpdates};

use super::*;

struct MockTransport {
    searches: Mutex<Vec<WebSearchRequest>>,
    extracts: Mutex<Vec<WebExtractRequest>>,
    search_response: WebSearchResponse,
    extract_response: WebExtractResponse,
    wait_for_cancellation: bool,
}

#[async_trait::async_trait]
impl MapleWebTransport for MockTransport {
    async fn web_search(
        self: Arc<Self>,
        request: WebSearchRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<WebSearchResponse> {
        self.searches.lock().unwrap().push(request);
        if self.wait_for_cancellation {
            cancel_token.cancelled().await;
            return Err(maple_sdk::Error::Other("cancelled".to_string()));
        }
        Ok(self.search_response.clone())
    }

    async fn web_extract(
        self: Arc<Self>,
        request: WebExtractRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<WebExtractResponse> {
        self.extracts.lock().unwrap().push(request);
        if self.wait_for_cancellation {
            cancel_token.cancelled().await;
            return Err(maple_sdk::Error::Other("cancelled".to_string()));
        }
        Ok(self.extract_response.clone())
    }
}

fn mock_transport() -> MockTransport {
    MockTransport {
        searches: Mutex::new(Vec::new()),
        extracts: Mutex::new(Vec::new()),
        search_response: WebSearchResponse {
            trace_id: None,
            results: vec![WebSearchResult {
                category: "search".to_string(),
                url: "https://example.com/result".to_string(),
                title: "Example".to_string(),
                snippet: Some("A result".to_string()),
                published_at: None,
            }],
        },
        extract_response: WebExtractResponse {
            trace_id: None,
            pages: vec![WebExtractPage {
                url: "https://example.com/result".to_string(),
                markdown: Some("Page text".to_string()),
                error: None,
            }],
        },
        wait_for_cancellation: false,
    }
}

fn search_params() -> WebSearchParams {
    WebSearchParams {
        query: "maple privacy".to_string(),
        workflow: None,
        page: None,
        limit: None,
        safe_search: None,
        lens_id: None,
        lens: None,
        filters: None,
    }
}

#[test]
fn the_schemas_do_not_expose_the_provider_timeout() {
    for tool in [web_search_declaration(), open_url_declaration()] {
        assert!(tool.parameters["properties"].get("timeout").is_none());
    }
}

#[test]
fn urls_are_normalized_as_the_backend_does() {
    assert_eq!(
        normalize_public_https_url("https://Example.com:443/page#fragment").unwrap(),
        "https://example.com/page"
    );
    for invalid in [
        "http://example.com",
        "https://localhost/page",
        "https://metadata.google.internal/latest",
        "https://127.0.0.1/page",
        "https://169.254.169.254/latest/meta-data/",
        "https://[::1]/page",
        "https://user:password@example.com/page",
        "https://example.com\n.evil.test/page",
        "https://example.com\t.evil.test/page",
        "https://example.com\u{0085}.evil.test/page",
    ] {
        assert!(normalize_public_https_url(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn ipv6_literals_follow_the_public_host_policy() {
    assert_eq!(
        normalize_public_https_url("https://[2606:4700::1111]/page").unwrap(),
        "https://[2606:4700::1111]/page"
    );
    for private in ["https://[fc00::1]/page", "https://[fe80::1]/page"] {
        assert!(normalize_public_https_url(private).is_err(), "{private}");
    }
}

#[tokio::test]
async fn a_search_calls_the_transport_once() {
    let concrete = Arc::new(mock_transport());
    let transport: Arc<dyn MapleWebTransport> = concrete.clone();
    let output = execute_web_search(&transport, search_params(), CancellationToken::new())
        .await
        .unwrap();
    assert!(output.contains("https://example.com/result"));
    assert_eq!(concrete.searches.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_bounded_search_stays_valid_json() {
    let mut concrete = mock_transport();
    concrete.search_response.trace_id = Some("t".repeat(MAX_TRACE_ID_CHARS + 100));
    concrete.search_response.results = (0..50)
        .map(|index| WebSearchResult {
            category: "search".to_string(),
            url: format!("https://example.com/{index}/{}", "a".repeat(1_800)),
            title: "t".repeat(300),
            snippet: Some("s".repeat(800)),
            published_at: None,
        })
        .collect();
    let transport: Arc<dyn MapleWebTransport> = Arc::new(concrete);
    let output = execute_web_search(&transport, search_params(), CancellationToken::new())
        .await
        .unwrap();
    assert!(output.chars().count() <= MAX_WEB_SEARCH_TOOL_OUTPUT_CHARS);
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["maple_truncated"], true);
    assert_eq!(
        value["trace_id"].as_str().unwrap().chars().count(),
        MAX_TRACE_ID_CHARS
    );
    assert!(value["trace_id"].as_str().unwrap().ends_with('…'));
    assert!(value["notice"].as_str().unwrap().contains("Untrusted"));
    assert!(value["results"].as_array().unwrap().len() < 50);
}

#[tokio::test]
async fn a_cancelled_search_is_reported_as_cancelled() {
    let transport: Arc<dyn MapleWebTransport> = Arc::new(MockTransport {
        wait_for_cancellation: true,
        ..mock_transport()
    });
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        execute_web_search(&transport, search_params(), cancel)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn open_url_extracts_one_normalized_url_and_bounds_its_text() {
    let mut concrete = mock_transport();
    concrete.extract_response.trace_id = Some("extract-trace".to_string());
    concrete.extract_response.pages[0].markdown = Some("🦀".repeat(40_000));
    let concrete = Arc::new(concrete);
    let transport: Arc<dyn MapleWebTransport> = concrete.clone();
    let output = execute_open_url(
        &transport,
        OpenUrlParams {
            url: "https://Example.com:443/result#ignored".to_string(),
            purpose: "Read the primary source".to_string(),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let extracts = concrete.extracts.lock().unwrap();
    assert_eq!(extracts.len(), 1);
    assert_eq!(extracts[0].urls, ["https://example.com/result"]);
    assert!(output.contains(OPEN_URL_TRUNCATION_MARKER.trim()));
    assert!(output.starts_with("Untrusted web-page evidence"));
    assert!(output.contains("Source: https://example.com/result"));
    assert!(output.contains("Trace ID: extract-trace"));
    assert!(output.contains("Content truncated by Maple: yes"));
    assert_eq!(output.chars().count(), MAX_OPEN_URL_TOOL_OUTPUT_CHARS);
}

#[test]
fn a_small_page_keeps_its_metadata_without_truncation() {
    let output = format_open_url_tool_output(
        "https://example.com/result",
        Some("extract-trace"),
        "Complete page text",
    );
    assert!(output.contains("Source: https://example.com/result"));
    assert!(output.contains("Trace ID: extract-trace"));
    assert!(output.contains("Content truncated by Maple: no"));
    assert!(output.ends_with("Complete page text"));
    assert!(!output.contains(OPEN_URL_TRUNCATION_MARKER.trim()));
}

#[test]
fn an_oversized_trace_id_is_bounded() {
    let output = format_open_url_tool_output(
        "https://example.com/result",
        Some(&"t".repeat(MAX_OPEN_URL_TOOL_OUTPUT_CHARS + 100)),
        "Complete page text",
    );
    assert!(output.chars().count() <= MAX_OPEN_URL_TOOL_OUTPUT_CHARS);
    assert!(output.contains(&format!(
        "Trace ID: {}…",
        "t".repeat(MAX_TRACE_ID_CHARS - 1)
    )));
    assert!(output.ends_with("Complete page text"));
}

#[test]
fn a_cut_page_does_not_turn_code_back_into_an_image() {
    let image_url = "https://images.example/reactivated.png";
    let code = format!("`![Inert code image]({image_url})`");
    let value = format!(
        "{code}{}",
        "x".repeat(OPEN_URL_TRUNCATION_MARKER.chars().count() + 10)
    );
    let closing_backtick = value.rfind('`').unwrap();
    let max_chars =
        value[..closing_backtick].chars().count() + OPEN_URL_TRUNCATION_MARKER.chars().count();

    let bounded = truncate_sanitized_markdown(&value, max_chars, OPEN_URL_TRUNCATION_MARKER);

    assert!(bounded.chars().count() <= max_chars);
    assert!(bounded.ends_with(OPEN_URL_TRUNCATION_MARKER));
    assert!(!bounded.contains(image_url));
    assert!(
        !Parser::new_ext(&bounded, Options::all())
            .any(|event| matches!(event, Event::Start(Tag::Image { .. })))
    );
}

#[test]
fn errors_fit_their_tools_output_limits() {
    for limit in [
        MAX_WEB_SEARCH_TOOL_OUTPUT_CHARS,
        MAX_OPEN_URL_TOOL_OUTPUT_CHARS,
    ] {
        let bounded = bound_web_tool_error("x".repeat(limit + 10), limit);
        assert_eq!(bounded.chars().count(), limit);
        assert!(bounded.ends_with(WEB_TOOL_ERROR_TRUNCATION_MARKER));
    }
}

#[tokio::test]
async fn the_tools_answer_the_model_with_results_and_errors() {
    let tools = web_tools(Arc::new(mock_transport()), true);
    let invoke = |index: usize, args: Value| {
        let tool = tools[index].tool.clone();
        async move {
            tool.execute(ToolInvocation {
                call_id: "call-1".to_string(),
                args,
                cancel: CancellationToken::new(),
                updates: ToolUpdates::none(),
            })
            .await
            .unwrap()
        }
    };
    let found = invoke(0, json!({"query": "maple"})).await;
    assert!(!found.is_error);
    assert!(pi_ai::content_text(&found.content).contains("https://example.com/result"));
    let refused = invoke(
        1,
        json!({"url": "https://localhost/x", "purpose": "Read it"}),
    )
    .await;
    assert!(refused.is_error);
    assert_eq!(
        pi_ai::content_text(&refused.content),
        "URL must include a public host"
    );
    assert!(tools.iter().all(|tool| tool.active));
    assert!(
        web_tools(Arc::new(mock_transport()), false)
            .iter()
            .all(|tool| !tool.active)
    );
}
