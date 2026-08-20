//! Integration tier: no credentials, no Google network.
//!
//! Every upstream here is a real in-process server (plan section 4-P5): the MCP
//! endpoints are genuine rmcp servers speaking the real protocol, and the one
//! plain-HTTP stub is Service Usage, which plan section 7 allows because it is
//! an ordinary REST API. Nothing in this file fakes a Google response shape
//! except where a verbatim captured error body is replayed on purpose.
//!
//! Criterion mapping (plan section 5):
//!
//! | Criterion | Test |
//! |---|---|
//! | 5.4 credential-free catalog | `catalog_builds_with_no_credentials_in_the_environment` |
//! | 5.4 embedded snapshot | `embedded_snapshot_loads_with_no_environment` |
//! | 5.5 dispatch round-trip | `dispatch_delivers_both_auth_headers_and_returns_the_result_unmodified` |
//! | 5.5 result passthrough | `dispatch_passes_an_upstream_error_result_through_unchanged` |
//! | 5.6 pruning | `only_the_enabled_services_are_exposed` |
//! | degradation | `a_downed_upstream_is_served_from_the_snapshot_until_refresh_succeeds` |
//! | T5 finding (b) | `a_disk_loaded_snapshot_never_reports_itself_as_live` |
//! | 5.8 log hygiene | `no_log_line_at_any_level_contains_the_token` |
//! | error mapping | `a_service_disabled_403_reaches_the_caller_as_an_enable_command` |
//! | serve-assembly provenance | `a_freshly_assembled_serve_catalog_reports_snapshot_provenance` |
//! | flat surface stability | `flat_mode_keeps_the_startup_tool_list_across_a_refresh` |
//! | pagination guard | `a_pagination_token_that_repeats_ends_the_listing_with_an_error` |
//! | outbound header hygiene | `a_dispatch_attaches_only_the_two_auth_headers_despite_promotion_annotations` |

mod common;

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use hyper::StatusCode;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, JsonObject},
};
use serde_json::{Value, json};

use mcp_google_service::{
    auth::{AuthContext, FetchedToken, TokenSource},
    catalog::{Catalog, CatalogSource, ServiceCatalog},
    config::ExposeMode,
    error::Error,
    proxy::{Proxy, Route},
    prune, registry,
    server::{CatalogState, GoogleMcpServer, assemble_serve_catalog},
};

use common::{
    PROMOTED_HEADER, SERVICE_DISABLED_BODY, TOOL_ECHO, TOOL_FAIL, TOOL_PROMOTES_HEADERS,
    TOOL_SHOW_HEADERS, client_resolving, scrub_google_credential_env, service_usage_page,
    spawn_failing_upstream, spawn_mcp_upstream, spawn_service_usage_stub,
};

/// Quota project used by every test that needs one. Never a real project.
const TEST_PROJECT: &str = "test-project";

/// A token source returning a fixed value, so no credentials are involved.
struct FixedToken(&'static str);

impl TokenSource for FixedToken {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
        let value = zeroize::Zeroizing::new(self.0.to_owned());
        Box::pin(async move {
            Ok(FetchedToken {
                value,
                expires_at: None,
            })
        })
    }
}

/// Auth context over a fixed token, for the given quota project.
fn fake_auth(token: &'static str, project: &str) -> Arc<AuthContext> {
    Arc::new(
        AuthContext::with_source(Arc::new(FixedToken(token)), project)
            .expect("a literal token and project id are valid header values"),
    )
}

/// Concatenated text of a tool result, for assertions.
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A live two-tier MCP session against `handler`, over an in-memory duplex.
///
/// Driving the server through a real client connection rather than calling
/// `ServerHandler` methods directly means the assertions cover the actual
/// protocol path: initialize, tool listing, and `tools/call` framing.
struct Session {
    client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    server: tokio::task::JoinHandle<()>,
}

impl Session {
    async fn connect(handler: GoogleMcpServer) -> Self {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            match handler.serve(server_io).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(error) => panic!("the in-memory MCP server failed to start: {error}"),
            }
        });
        let client = ()
            .serve(client_io)
            .await
            .expect("the in-memory MCP client completes the initialize handshake");
        Self { client, server }
    }

    /// Invoke a two-tier meta-tool.
    async fn call(&self, name: &str, arguments: Value) -> CallToolResult {
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = arguments.as_object().cloned();
        self.client
            .call_tool(params)
            .await
            .unwrap_or_else(|error| panic!("calling `{name}` over the session failed: {error}"))
    }

    /// Tool names the server advertises, as the client sees them.
    async fn list_tool_names(&self) -> Vec<String> {
        self.client
            .list_all_tools()
            .await
            .expect("listing tools over the session succeeds")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    /// Invoke the two-tier `call` tool against a namespaced upstream tool.
    async fn dispatch(&self, target: &str, arguments: Value) -> CallToolResult {
        self.call("call", json!({ "name": target, "arguments": arguments }))
            .await
    }

    /// Close the session and make sure no server task outlives the test.
    async fn shutdown(self) {
        let _ = self.client.cancel().await;
        let _ = self.server.await;
    }
}

/// Wrap a catalog in the state holder the server expects.
fn shared(catalog: Catalog) -> CatalogState {
    CatalogState::new(catalog)
}

// ---------------------------------------------------------------------------
// Section 5.4 -- credential-free catalog
// ---------------------------------------------------------------------------

/// Discovery must work with an environment holding no Google credentials at
/// all, and must not send any credential of its own.
#[tokio::test]
async fn catalog_builds_with_no_credentials_in_the_environment() {
    let scrubbed = scrub_google_credential_env();

    let upstream = spawn_mcp_upstream(
        &["run.googleapis.com", "bigquery.googleapis.com"],
        "synthetic-discovery",
    )
    .await;
    let addr = upstream.server.addr();
    let http = client_resolving(&[
        ("run.googleapis.com", addr),
        ("bigquery.googleapis.com", addr),
    ]);

    let endpoints = vec![
        registry::find("run").expect("`run` is a registered endpoint"),
        registry::find("bigquery").expect("`bigquery` is a registered endpoint"),
    ];
    let catalog = Catalog::build_live(endpoints, &http, None)
        .await
        .expect("discovery against the in-process upstreams yields a valid catalog");

    assert_eq!(
        catalog.services.len(),
        2,
        "both upstreams answered, so both services must be present (scrubbed env vars: {scrubbed:?}); \
         got services {:?}",
        catalog
            .services
            .iter()
            .map(|s| &s.service_id)
            .collect::<Vec<_>>()
    );
    for service in &catalog.services {
        assert_eq!(
            service.source,
            CatalogSource::Live,
            "service `{}` was fetched live, so it must be labelled live",
            service.service_id
        );
    }
    assert!(
        catalog.get(&format!("run__{TOOL_ECHO}")).is_some(),
        "the synthetic `{TOOL_ECHO}` tool must appear namespaced under `run`; catalog holds {:?}",
        catalog
            .tools()
            .map(|t| &t.namespaced_name)
            .collect::<Vec<_>>()
    );

    // The strong form of "credential-free": nothing resembling a credential
    // was put on the wire during discovery.
    let requests = upstream.all_requests();
    assert!(
        !requests.is_empty(),
        "the upstream must have observed the discovery requests"
    );
    for (index, headers) in requests.iter().enumerate() {
        assert!(
            !headers.contains_key("authorization"),
            "discovery request {index} carried an Authorization header; \
             unauthenticated discovery is the property this criterion asserts. Headers: {headers:?}"
        );
        assert!(
            !headers.contains_key("x-goog-user-project"),
            "discovery request {index} carried a quota-project header. Headers: {headers:?}"
        );
    }

    upstream.server.shutdown().await;
}

/// The snapshot compiled into the binary must load with nothing in the
/// environment and no file on disk to help it.
#[tokio::test]
async fn embedded_snapshot_loads_with_no_environment() {
    let scrubbed = scrub_google_credential_env();

    let snapshot = mcp_google_service::catalog::embedded_snapshot()
        .expect("the snapshot embedded at compile time must always parse");
    let catalog = snapshot
        .into_catalog()
        .expect("the embedded snapshot must satisfy the namespacing invariants");

    assert!(
        !catalog.services.is_empty(),
        "the embedded snapshot must carry services (scrubbed env vars: {scrubbed:?})"
    );
    assert!(
        catalog.tool_count() > 0,
        "the embedded snapshot must carry tools; got {} services and 0 tools",
        catalog.services.len()
    );
}

// ---------------------------------------------------------------------------
// Section 5.5 -- dispatch round-trip
// ---------------------------------------------------------------------------

/// Both auth headers must arrive at the upstream, and the upstream's result
/// must come back to the caller unchanged.
#[tokio::test]
async fn dispatch_delivers_both_auth_headers_and_returns_the_result_unmodified() {
    const TOKEN: &str = "integration-token-round-trip";

    let upstream = spawn_mcp_upstream(&["run.googleapis.com"], "synthetic-run").await;
    let addr = upstream.server.addr();
    let http = client_resolving(&[("run.googleapis.com", addr)]);

    let run = registry::find("run").expect("`run` is a registered endpoint");
    let catalog = Catalog::build_live(vec![run], &http, None)
        .await
        .expect("the upstream tool list builds a catalog");

    let proxy = Proxy::new(
        fake_auth(TOKEN, TEST_PROJECT),
        http,
        // The production route: `https://run.googleapis.com/mcp`, reached
        // through the DNS override rather than a rewritten URL.
        vec![Route::from_endpoint(run)],
    );
    let session = Session::connect(GoogleMcpServer::new(
        shared(catalog),
        Arc::new(proxy),
        ExposeMode::TwoTier,
    ))
    .await;

    let payload = "round-trip-payload";
    let result = session
        .dispatch(&format!("run__{TOOL_ECHO}"), json!({ "payload": payload }))
        .await;

    assert_ne!(
        result.is_error,
        Some(true),
        "a successful upstream call must not be reported as an error; result text: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    let echoed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the echoed body must be JSON ({error}); got: {text}"));
    assert_eq!(
        echoed,
        json!({ "payload": payload }),
        "the upstream result must be returned unmodified"
    );

    // Assert the headers as the upstream saw them, not as the proxy claims.
    let calls: Vec<_> = upstream
        .all_requests()
        .into_iter()
        .filter(|headers| headers.contains_key("authorization"))
        .collect();
    assert!(
        !calls.is_empty(),
        "no request carried an Authorization header; the upstream observed: {:?}",
        upstream.all_requests()
    );
    for headers in &calls {
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str()),
            "the bearer token must reach the upstream verbatim; headers: {headers:?}"
        );
        assert_eq!(
            headers.get("x-goog-user-project").map(String::as_str),
            Some(TEST_PROJECT),
            "the quota project must accompany every authenticated call; headers: {headers:?}"
        );
    }

    let observed = upstream.last_headers();
    session.shutdown().await;
    upstream.server.shutdown().await;

    assert!(
        observed.contains_key("authorization"),
        "the final dispatch request must have been authenticated"
    );
}

/// An upstream that answers `isError: true` must reach the caller as an error
/// result, not be reshaped into a success or a protocol error.
#[tokio::test]
async fn dispatch_passes_an_upstream_error_result_through_unchanged() {
    let upstream = spawn_mcp_upstream(&["run.googleapis.com"], "synthetic-run").await;
    let addr = upstream.server.addr();
    let http = client_resolving(&[("run.googleapis.com", addr)]);

    let run = registry::find("run").expect("`run` is a registered endpoint");
    let catalog = Catalog::build_live(vec![run], &http, None)
        .await
        .expect("the upstream tool list builds a catalog");
    let proxy = Proxy::new(
        fake_auth("token-for-error-passthrough", TEST_PROJECT),
        http,
        vec![Route::from_endpoint(run)],
    );
    let session = Session::connect(GoogleMcpServer::new(
        shared(catalog),
        Arc::new(proxy),
        ExposeMode::TwoTier,
    ))
    .await;

    let result = session
        .dispatch(&format!("run__{TOOL_FAIL}"), json!({}))
        .await;

    assert_eq!(
        result.is_error,
        Some(true),
        "the upstream's isError flag must survive the proxy; result text: {}",
        result_text(&result)
    );
    assert!(
        result_text(&result).contains("synthetic upstream failure"),
        "the upstream's own message must survive the proxy; got: {}",
        result_text(&result)
    );

    session.shutdown().await;
    upstream.server.shutdown().await;
}

/// The headers the upstream reports back must match what the proxy attached,
/// giving a second, in-band witness for the section 5.5 header assertion.
#[tokio::test]
async fn the_upstream_can_read_back_the_headers_the_proxy_attached() {
    const TOKEN: &str = "integration-token-readback";

    let upstream = spawn_mcp_upstream(&["run.googleapis.com"], "synthetic-run").await;
    let addr = upstream.server.addr();
    let http = client_resolving(&[("run.googleapis.com", addr)]);

    let run = registry::find("run").expect("`run` is a registered endpoint");
    let catalog = Catalog::build_live(vec![run], &http, None)
        .await
        .expect("the upstream tool list builds a catalog");
    let proxy = Proxy::new(
        fake_auth(TOKEN, TEST_PROJECT),
        http,
        vec![Route::from_endpoint(run)],
    );
    let session = Session::connect(GoogleMcpServer::new(
        shared(catalog),
        Arc::new(proxy),
        ExposeMode::TwoTier,
    ))
    .await;

    let result = session
        .dispatch(&format!("run__{TOOL_SHOW_HEADERS}"), json!({}))
        .await;
    let text = result_text(&result);
    let reported: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("header readback must be JSON ({error}); got: {text}"));

    assert_eq!(
        reported.get("authorization").and_then(Value::as_str),
        Some(format!("Bearer {TOKEN}").as_str()),
        "the upstream must have received the bearer token; it reported: {reported}"
    );
    assert_eq!(
        reported.get("x-goog-user-project").and_then(Value::as_str),
        Some(TEST_PROJECT),
        "the upstream must have received the quota project; it reported: {reported}"
    );

    session.shutdown().await;
    upstream.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Section 5.6 -- Service Usage pruning
// ---------------------------------------------------------------------------

/// Given a Service Usage listing of exactly two enabled APIs, spread over two
/// pages, exactly those two services' tools may be exposed.
#[tokio::test]
async fn only_the_enabled_services_are_exposed() {
    // Two pages, so the pagination chain must actually be followed to see
    // both APIs. Page 0 points at page 1; page 1 terminates the listing.
    let stub = spawn_service_usage_stub(vec![
        service_usage_page(&["run.googleapis.com"], Some("page-1")),
        service_usage_page(&["bigquery.googleapis.com"], None),
    ])
    .await;
    let http = client_resolving(&[("serviceusage.googleapis.com", stub.addr())]);
    let auth = fake_auth("token-for-pruning", TEST_PROJECT);

    let enabled = prune::enabled_services(&auth, TEST_PROJECT, &http)
        .await
        .expect("the Service Usage stub returns a well-formed listing");

    assert_eq!(
        enabled.len(),
        2,
        "both pages must be read; the second page is only reachable by \
         following nextPageToken. Got: {enabled:?}"
    );
    assert!(
        enabled.contains("run.googleapis.com") && enabled.contains("bigquery.googleapis.com"),
        "both enabled APIs must be collected; got: {enabled:?}"
    );

    let exposed = prune::select_services(registry::ENDPOINTS, Some(&enabled), &[], &[]);
    let exposed_ids: Vec<&str> = exposed.iter().map(|e| e.service_id).collect();
    assert_eq!(
        exposed_ids,
        vec!["run", "bigquery"],
        "only the enabled services may be exposed, in registry order"
    );

    // Now prove the restriction reaches the model-facing surface: build a
    // catalog covering more services than are enabled, restrict it, and ask
    // the server what it exposes.
    let full = Catalog::new(vec![
        synthetic_service("run", CatalogSource::Snapshot),
        synthetic_service("bigquery", CatalogSource::Snapshot),
        synthetic_service("compute", CatalogSource::Snapshot),
        synthetic_service("logging", CatalogSource::Snapshot),
    ])
    .expect("synthetic services satisfy the namespacing invariants");
    assert_eq!(
        full.services.len(),
        4,
        "the pre-pruning catalog must be strictly larger than the enabled set"
    );

    let pruned = full.restricted_to(&exposed);
    let proxy = Proxy::new(
        fake_auth("token-for-pruning", TEST_PROJECT),
        http,
        exposed.iter().map(|e| Route::from_endpoint(e)).collect(),
    );
    let session = Session::connect(GoogleMcpServer::new(
        shared(pruned),
        Arc::new(proxy),
        ExposeMode::TwoTier,
    ))
    .await;

    let listed = session.call("list_services", json!({})).await;
    let listed_json: Value =
        serde_json::from_str(&result_text(&listed)).expect("list_services returns a JSON payload");
    let listed_ids: Vec<&str> = listed_json["services"]
        .as_array()
        .expect("list_services reports a services array")
        .iter()
        .filter_map(|s| s["service_id"].as_str())
        .collect();
    assert_eq!(
        listed_ids,
        vec!["bigquery", "run"],
        "list_services must show exactly the enabled services (catalogs are \
         sorted by service id); full payload: {listed_json}"
    );

    // Search must not reach a disabled service either.
    let hits = session
        .call("search_tools", json!({ "query": "synthetic", "limit": 50 }))
        .await;
    let hits_json: Value =
        serde_json::from_str(&result_text(&hits)).expect("search_tools returns a JSON payload");
    let hit_names: Vec<&str> = hits_json["matches"]
        .as_array()
        .expect("search_tools reports a matches array")
        .iter()
        .filter_map(|hit| hit["name"].as_str())
        .collect();
    assert!(
        !hit_names.is_empty(),
        "the synthetic tools must be searchable; payload: {hits_json}"
    );
    for name in &hit_names {
        let service = name.split("__").next().unwrap_or_default();
        assert!(
            service == "run" || service == "bigquery",
            "search surfaced `{name}` from disabled service `{service}`; \
             only enabled services may be reachable. Payload: {hits_json}"
        );
    }

    session.shutdown().await;
    stub.shutdown().await;
}

/// A service catalog with one recognisable tool, for pruning assertions.
fn synthetic_service(service_id: &str, source: CatalogSource) -> ServiceCatalog {
    use rmcp::model::Tool;

    let schema = Arc::new(JsonObject::new());
    let tool = Tool::new(
        "synthetic_probe",
        "A synthetic tool used to prove which services are exposed.",
        schema,
    );
    ServiceCatalog {
        service_id: service_id.to_owned(),
        source,
        tools: vec![mcp_google_service::catalog::NamespacedTool::new(
            service_id, tool,
        )],
    }
}

// ---------------------------------------------------------------------------
// Degradation and provenance
// ---------------------------------------------------------------------------

/// A dead upstream with a snapshot entry must be served from the snapshot and
/// labelled as such; once the upstream returns, a refresh must flip it to live.
#[tokio::test]
async fn a_downed_upstream_is_served_from_the_snapshot_until_refresh_succeeds() {
    let run = registry::find("run").expect("`run` is a registered endpoint");

    // A snapshot that already knows about `run`.
    let snapshot = Catalog::new(vec![synthetic_service("run", CatalogSource::Snapshot)])
        .expect("the synthetic snapshot is valid");

    // Phase 1: nothing is listening on the address the endpoint resolves to.
    // The upstream is started and then shut down, rather than a port being
    // guessed: that yields an address that is reliably closed and reliably not
    // in use by anything else.
    let dead = spawn_mcp_upstream(&["run.googleapis.com"], "about-to-die").await;
    let dead_addr = dead.server.addr();
    dead.server.shutdown().await;

    let http_down = client_resolving(&[("run.googleapis.com", dead_addr)]);
    let degraded = Catalog::build_live(vec![run], &http_down, Some(&snapshot))
        .await
        .expect("a failing upstream degrades rather than erroring");

    let service = degraded
        .service("run")
        .expect("the snapshot entry must keep the service present when the upstream is down");
    assert_eq!(
        service.source,
        CatalogSource::Snapshot,
        "a service served from the snapshot must be labelled snapshot, so \
         `list_services` can tell the operator the data is stale"
    );
    assert_eq!(
        service.tools.len(),
        1,
        "the snapshot's tools must be carried over verbatim"
    );

    // Phase 2: the upstream is back; a refresh must take over and relabel.
    let alive = spawn_mcp_upstream(&["run.googleapis.com"], "revived-run").await;
    let http_up = client_resolving(&[("run.googleapis.com", alive.server.addr())]);
    let refreshed = Catalog::build_live(vec![run], &http_up, Some(&degraded))
        .await
        .expect("the revived upstream answers discovery");

    let service = refreshed
        .service("run")
        .expect("the revived upstream must be present");
    assert_eq!(
        service.source,
        CatalogSource::Live,
        "once the upstream answers, its entry must be relabelled live"
    );
    assert!(
        service.tools.len() >= 3,
        "the live fetch must replace the single snapshot tool with the \
         upstream's real toolset; got {} tools",
        service.tools.len()
    );

    alive.server.shutdown().await;
}

/// Regression for T5 finding (b): a catalog restored from disk must never
/// claim to be live, whatever provenance the file recorded when written.
#[tokio::test]
async fn a_disk_loaded_snapshot_never_reports_itself_as_live() {
    let snapshot =
        mcp_google_service::catalog::embedded_snapshot().expect("the embedded snapshot must parse");
    let loaded = snapshot
        .into_catalog()
        .expect("the committed snapshot must satisfy the namespacing invariants");

    // The file records `live`, because that is what it was when captured.
    // This is precisely why the relabel below is load-bearing rather than
    // cosmetic: without it, a snapshot-served process reports fresh data.
    let live_in_file = loaded
        .services
        .iter()
        .filter(|s| s.source == CatalogSource::Live)
        .count();
    assert!(
        live_in_file > 0,
        "the committed snapshot is expected to record live provenance from \
         its capture run; if this ever fails, the relabel regression this \
         test guards has changed shape and the test must be revisited"
    );

    let relabelled = loaded.marked_as(CatalogSource::Snapshot);
    for service in &relabelled.services {
        assert_eq!(
            service.source,
            CatalogSource::Snapshot,
            "service `{}` came off disk, so it must report snapshot until a \
             live refresh actually lands",
            service.service_id
        );
    }
    assert_eq!(
        relabelled.tool_count(),
        loaded.tool_count(),
        "relabelling provenance must not change the catalog's contents"
    );
}

/// The serve assembly must label what it serves as snapshot-sourced.
///
/// Regression guard for the provenance relabel: it is one call in the middle
/// of the serve path, it has no effect a compiler can check, and dropping it
/// makes a stale catalog report itself as freshly fetched. The assertion runs
/// against the real committed snapshot, whose services record `live`, so the
/// relabel is the only thing that can produce the expected result.
#[tokio::test]
async fn a_freshly_assembled_serve_catalog_reports_snapshot_provenance() {
    let snapshot =
        mcp_google_service::catalog::embedded_snapshot().expect("the embedded snapshot must parse");
    assert!(
        snapshot
            .services
            .iter()
            .any(|service| service.source == CatalogSource::Live),
        "the committed snapshot is expected to record live provenance from its \
         capture run; without that this test cannot distinguish a relabel from \
         a passthrough and must be revisited"
    );

    let exposed = vec![
        registry::find("run").expect("`run` is a registered endpoint"),
        registry::find("bigquery").expect("`bigquery` is a registered endpoint"),
    ];
    let state = assemble_serve_catalog(snapshot, &exposed)
        .expect("the committed snapshot satisfies the namespacing invariants");

    let startup = state.startup();
    assert_eq!(
        startup.services.len(),
        2,
        "the assembly must narrow the snapshot to the exposed endpoints"
    );
    for service in &startup.services {
        assert_eq!(
            service.source,
            CatalogSource::Snapshot,
            "service `{}` has not been fetched by this process, so a catalog \
             assembled for serving must say snapshot until a refresh lands",
            service.service_id
        );
    }

    // And the same through the model-facing surface, which is where an
    // operator actually reads provenance.
    let session = Session::connect(GoogleMcpServer::new(
        state,
        Arc::new(Proxy::new(
            fake_auth("token-for-provenance", TEST_PROJECT),
            client_resolving(&[]),
            Vec::new(),
        )),
        ExposeMode::TwoTier,
    ))
    .await;
    let listed = session.call("list_services", json!({})).await;
    let payload: Value =
        serde_json::from_str(&result_text(&listed)).expect("list_services returns JSON");
    for service in payload["services"]
        .as_array()
        .expect("list_services reports a services array")
    {
        assert_eq!(
            service["source"],
            json!("snapshot"),
            "list_services must not overstate freshness: {payload}"
        );
    }
    session.shutdown().await;
}

/// `--expose flat` must keep serving the tool list it published at startup.
///
/// The client is handed concrete tool names at `initialize` and this server
/// sends no `listChanged`, so a refresh that moved the flat list would leave
/// the client offering tools the server no longer has and hiding tools it
/// does.
#[tokio::test]
async fn flat_mode_keeps_the_startup_tool_list_across_a_refresh() {
    let state = CatalogState::new(
        Catalog::new(vec![
            synthetic_service("run", CatalogSource::Snapshot),
            synthetic_service("bigquery", CatalogSource::Snapshot),
        ])
        .expect("synthetic services satisfy the namespacing invariants"),
    );
    let live = state.live();

    let session = Session::connect(GoogleMcpServer::new(
        state,
        Arc::new(Proxy::new(
            fake_auth("token-for-flat-freeze", TEST_PROJECT),
            client_resolving(&[]),
            Vec::new(),
        )),
        ExposeMode::Flat,
    ))
    .await;

    let names_before = session.list_tool_names().await;
    assert_eq!(
        names_before,
        vec!["bigquery__synthetic_probe", "run__synthetic_probe"],
        "flat mode registers every exposed tool by its namespaced name"
    );

    // A background refresh lands and drops `run` entirely.
    *live.write().await = Arc::new(
        Catalog::new(vec![synthetic_service("bigquery", CatalogSource::Live)])
            .expect("synthetic services satisfy the namespacing invariants"),
    );

    assert_eq!(
        session.list_tool_names().await,
        names_before,
        "the flat tool list is fixed at startup: the client cannot be told it \
         changed, so it must not change"
    );

    session.shutdown().await;
}

/// A Service Usage listing that never advances must be abandoned, not looped.
///
/// The stub answers `pageToken=page-0` with the same page, which carries
/// `page-0` again: a cursor that does not move. Without a guard this is an
/// endless request loop against Google, each iteration minting a fresh token.
#[tokio::test]
async fn a_pagination_token_that_repeats_ends_the_listing_with_an_error() {
    let stub = spawn_service_usage_stub(vec![service_usage_page(
        &["run.googleapis.com"],
        Some("page-0"),
    )])
    .await;
    let http = client_resolving(&[("serviceusage.googleapis.com", stub.addr())]);
    let auth = fake_auth("token-for-stalled-pagination", TEST_PROJECT);

    let error = prune::enabled_services(&auth, TEST_PROJECT, &http)
        .await
        .expect_err("a listing that never advances must fail rather than spin");
    assert!(
        matches!(error, Error::PaginationStalled),
        "expected the repeated-token guard to fire; got: {error}"
    );

    stub.shutdown().await;
}

/// Only our two auth headers may be attached, whatever the upstream's schema
/// asks for.
///
/// rmcp implements SEP-2243 `x-mcp-header` promotion on the client transport:
/// an upstream can annotate a tool parameter and have the client copy that
/// argument into an HTTP request header. The upstream writes those schemas, so
/// that is an upstream-controlled instruction acting on the same header space
/// that carries the access token.
///
/// Promotion is currently unreachable for two independent reasons, and this
/// test exists because both are rmcp's to change, not ours: emission is gated
/// on `ProtocolVersion::STANDARD_HEADERS` (2026-07-28) while rmcp's client
/// still requests 2025-11-25, and promotion additionally needs the tool's
/// schema in the transport's cache, which only a `tools/list` on the dispatch
/// client would populate. So this pins what the real dispatch path puts on the
/// wire: exactly our two auth headers, and no argument data anywhere among
/// them.
#[tokio::test]
async fn a_dispatch_attaches_only_the_two_auth_headers_despite_promotion_annotations() {
    const TOKEN: &str = "integration-token-header-promotion";
    const SENTINEL: &str = "argument-value-that-must-stay-in-the-body";

    let upstream = spawn_mcp_upstream(&["run.googleapis.com"], "synthetic-annotated").await;
    let addr = upstream.server.addr();
    let http = client_resolving(&[("run.googleapis.com", addr)]);

    let run = registry::find("run").expect("`run` is a registered endpoint");
    let catalog = Catalog::build_live(vec![run], &http, None)
        .await
        .expect("the upstream tool list builds a catalog");
    assert!(
        catalog
            .get(&format!("run__{TOOL_PROMOTES_HEADERS}"))
            .is_some(),
        "the annotated tool must be in the catalog, or this test proves nothing"
    );

    let session = Session::connect(GoogleMcpServer::new(
        shared(catalog),
        Arc::new(Proxy::new(
            fake_auth(TOKEN, TEST_PROJECT),
            http,
            vec![Route::from_endpoint(run)],
        )),
        ExposeMode::TwoTier,
    ))
    .await;

    let result = session
        .dispatch(
            &format!("run__{TOOL_PROMOTES_HEADERS}"),
            json!({ "region": SENTINEL }),
        )
        .await;
    let reported = result_text(&result);
    session.shutdown().await;

    // What the upstream saw on the wire, not what the proxy believes it sent.
    let requests = upstream.all_requests();
    upstream.server.shutdown().await;

    // Recorded rather than asserted: the negotiated version is what decides
    // whether promotion is reachable at all, so a future rmcp that moves it
    // past 2026-07-28 should show up here while the assertions below keep
    // holding.
    let negotiated: Vec<&str> = requests
        .iter()
        .filter_map(|headers| headers.get("mcp-protocol-version"))
        .map(String::as_str)
        .collect();
    eprintln!("negotiated MCP protocol version(s): {negotiated:?}");

    let promoted = format!("mcp-param-{}", PROMOTED_HEADER.to_ascii_lowercase());
    for headers in &requests {
        assert!(
            !headers.contains_key(promoted.as_str()),
            "an argument was promoted into `{promoted}`; tool arguments must \
             stay in the request body, where they cannot interact with the \
             headers carrying our credentials. Headers: {headers:?}"
        );
        for (name, value) in headers {
            assert!(
                !value.contains(SENTINEL),
                "argument data reached header `{name}`: {value}"
            );
        }
    }

    // Headers the transport puts on every request regardless of what this
    // crate attaches. Anything outside this set and the two auth headers came
    // from us or from an argument, and both would be news.
    const TRANSPORT_HEADERS: &[&str] = &[
        "host",
        "accept",
        "content-type",
        "content-length",
        "user-agent",
        "mcp-session-id",
        "mcp-protocol-version",
    ];

    let authenticated: Vec<_> = requests
        .iter()
        .filter(|headers| headers.contains_key("authorization"))
        .collect();
    assert!(
        !authenticated.is_empty(),
        "no authenticated request reached the upstream: {requests:?}"
    );
    for headers in authenticated {
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(
            headers.get("x-goog-user-project").map(String::as_str),
            Some(TEST_PROJECT)
        );

        let ours: Vec<&str> = headers
            .keys()
            .map(String::as_str)
            .filter(|name| !TRANSPORT_HEADERS.contains(name))
            .collect();
        assert_eq!(
            ours.len(),
            2,
            "a dispatch must attach exactly `authorization` and \
             `x-goog-user-project`; found {ours:?} in {headers:?}"
        );
        assert!(
            ours.contains(&"authorization") && ours.contains(&"x-goog-user-project"),
            "the two attached headers must be the auth pair, not something \
             else: {ours:?}"
        );
    }

    assert!(
        reported.contains("authorization"),
        "the upstream reports the headers it received, so the result should \
         name them; got: {reported}"
    );
}

// ---------------------------------------------------------------------------
// Section 5.8 -- log hygiene
// ---------------------------------------------------------------------------

/// Buffer that a tracing subscriber writes into, so the test can inspect it.
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the capture buffer is never held across a panic")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// No log line at any level may contain the access token.
#[tokio::test]
async fn no_log_line_at_any_level_contains_the_token() {
    // A value that cannot occur by chance, so a hit is unambiguous.
    const SENTINEL: &str = "ya29-SENTINEL-do-not-log-1a2b3c4d5e6f";

    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(Arc::clone(&captured)))
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    // nextest runs each test in its own process, so a global subscriber here
    // captures spawned tasks too -- which a thread-local default would miss.
    tracing::subscriber::set_global_default(subscriber)
        .expect("no other subscriber is installed in this test process");

    let upstream = spawn_mcp_upstream(&["run.googleapis.com"], "synthetic-run").await;
    let addr = upstream.server.addr();
    let http = client_resolving(&[("run.googleapis.com", addr)]);
    let run = registry::find("run").expect("`run` is a registered endpoint");

    // Exercise the paths that handle the token: discovery, a successful
    // dispatch, a routing failure, and an upstream failure whose error text is
    // rendered into a message.
    let catalog = Catalog::build_live(vec![run], &http, None)
        .await
        .expect("discovery succeeds");
    let proxy = Proxy::new(
        fake_auth(SENTINEL, TEST_PROJECT),
        http,
        vec![Route::from_endpoint(run)],
    );
    let _ok = proxy
        .dispatch(&format!("run__{TOOL_ECHO}"), Some(JsonObject::new()))
        .await;
    let _unknown = proxy.dispatch("nosuchservice__tool", None).await;
    let _unnamespaced = proxy.dispatch("bare_name", None).await;
    let _failing = proxy
        .dispatch(&format!("run__{TOOL_FAIL}"), Some(JsonObject::new()))
        .await;
    drop(catalog);

    upstream.server.shutdown().await;

    let logs = String::from_utf8(
        captured
            .lock()
            .expect("the capture buffer is never held across a panic")
            .clone(),
    )
    .expect("tracing output is valid UTF-8");

    // A vacuous pass would be worse than a failure: prove logging happened.
    assert!(
        !logs.trim().is_empty(),
        "no log output was captured at all, so this test would pass \
         vacuously; the subscriber or the exercised paths are wrong"
    );
    assert!(
        !logs.contains(SENTINEL),
        "the access token leaked into logs. Offending lines:\n{}",
        logs.lines()
            .filter(|line| line.contains(SENTINEL))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The `Bearer ` prefix would be equally damaging on its own.
    assert!(
        !logs.contains("Bearer "),
        "a rendered Authorization header reached the logs:\n{}",
        logs.lines()
            .filter(|line| line.contains("Bearer "))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ---------------------------------------------------------------------------
// Error mapping, end to end
// ---------------------------------------------------------------------------

/// A 403 carrying Google's verbatim `SERVICE_DISABLED` body must reach the
/// caller as an error result naming the exact `gcloud services enable` command.
#[tokio::test]
async fn a_service_disabled_403_reaches_the_caller_as_an_enable_command() {
    let upstream = spawn_failing_upstream(
        &["run.googleapis.com"],
        StatusCode::FORBIDDEN,
        SERVICE_DISABLED_BODY,
    )
    .await;
    let http = client_resolving(&[("run.googleapis.com", upstream.addr())]);
    let run = registry::find("run").expect("`run` is a registered endpoint");

    let proxy = Proxy::new(
        fake_auth("token-for-service-disabled", TEST_PROJECT),
        http,
        vec![Route::from_endpoint(run)],
    );

    // Straight through the real dispatch path: the status survives only inside
    // rmcp's rendered error text (T5 finding (a)), so this exercises the
    // text-parse fallback as well as the classifier.
    let result = proxy
        .dispatch(&format!("run__{TOOL_ECHO}"), Some(JsonObject::new()))
        .await;

    assert_eq!(
        result.is_error,
        Some(true),
        "an upstream 403 must surface as an error result; got: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    assert!(
        text.contains("gcloud services enable run.googleapis.com --project=test-project-1234"),
        "the remediation must name the exact enabling command, with the API \
         and project parsed out of the upstream body; got: {text}"
    );
    assert!(
        !text.contains("<API>") && !text.contains("<PROJECT>"),
        "the API and project must be parsed from the body, not left as \
         placeholders; got: {text}"
    );

    upstream.shutdown().await;
}
