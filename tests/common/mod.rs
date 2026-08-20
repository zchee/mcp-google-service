//! Shared integration harness.
//!
//! # Why these upstreams are real servers
//!
//! Plan section 4-P5 requires the integration tier to talk to a **genuine**
//! rmcp MCP server rather than a canned-response fake of Google: the point is
//! to exercise the real protocol (initialize handshake, session ids, tool
//! listing, streamable-HTTP framing) so that a protocol-level regression is
//! caught here instead of in the live tier. The single deliberate exception is
//! the Service Usage stub, which plan section 7 permits as plain HTTP REST
//! because it is an ordinary JSON API, not an MCP endpoint.
//!
//! # Why the upstreams speak TLS
//!
//! `Catalog::build_live` reaches an endpoint through
//! `Endpoint::mcp_url()` -> `https://{host}/mcp`, and `prune::enabled_services`
//! builds `https://serviceusage.googleapis.com/v1/...`. Both hardcode the
//! scheme and the hostname, so neither can be aimed at `http://127.0.0.1`.
//! Rather than widen production APIs with a test-only base-URL parameter, the
//! tests override DNS on the reqwest client
//! ([`ClientBuilder::resolve`](reqwest::ClientBuilder::resolve)) so the real
//! hostnames resolve to a loopback socket. Production URL construction is then
//! part of what the tests verify: a wrong scheme, host, or path would fail
//! here.
//!
//! Certificates are generated in memory per run and the test client is told to
//! skip verification, so no private key is ever committed.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use http_body_util::{Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ConnBuilder,
    service::TowerToHyperService,
};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        Tool,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_rustls::TlsAcceptor;

/// Tool that echoes its arguments back, for round-trip assertions.
pub const TOOL_ECHO: &str = "echo_arguments";
/// Tool that reports the HTTP headers the upstream received on this session.
pub const TOOL_SHOW_HEADERS: &str = "show_received_headers";
/// Tool that always answers with `isError: true`, for passthrough assertions.
pub const TOOL_FAIL: &str = "always_fails";

/// Tool whose schema asks rmcp to promote an argument into an HTTP header.
///
/// SEP-2243 lets a tool annotate a top-level property with `x-mcp-header`, and
/// rmcp's client transport honours that by copying the argument into an
/// `Mcp-Param-*` request header once the negotiated protocol version is new
/// enough. An upstream writes its own schemas, so this is an upstream-supplied
/// instruction to move caller data into the header space that also carries our
/// credentials. It answers with the headers it received, so a test can see
/// exactly what arrived.
pub const TOOL_PROMOTES_HEADERS: &str = "promotes_headers";

/// Header the annotation on [`TOOL_PROMOTES_HEADERS`] asks for.
pub const PROMOTED_HEADER: &str = "Region";

/// Verbatim `SERVICE_DISABLED` body captured from Google on 2026-08-19.
///
/// Kept exact so the classifier is tested against the real wire text rather
/// than a paraphrase of it.
pub const SERVICE_DISABLED_BODY: &str = concat!(
    r#"{"error":{"code":403,"message":"Cloud Run Admin API has not been used in project "#,
    r#"test-project-1234 before or it is disabled. Enable it by visiting "#,
    r#"https://console.developers.google.com/apis/api/run.googleapis.com/overview?project=test-project-1234"#,
    r#" then retry.","status":"PERMISSION_DENIED"}}"#,
);

/// Install a rustls crypto provider exactly once per process.
///
/// rustls 0.23 is compiled here with both the `aws-lc-rs` and `ring` backends
/// (reqwest pulls one, rcgen the other), and with more than one available it
/// refuses to pick a default on its own -- building a `ServerConfig` would
/// panic. Choosing `ring` matches what rcgen already links.
fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // An error here means a provider was already installed by another
        // component, which is equally fine for our purposes.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Remove every Google credential hint from this process's environment.
///
/// Acceptance criterion section 5.4 requires discovery to work with **no**
/// credentials present, and requires the test to prove that rather than
/// inherit it from a developer's machine. Returns the names actually removed
/// so a test can report what it scrubbed.
///
/// # Safety and test-runner assumptions
///
/// `std::env::remove_var` is `unsafe` in edition 2024 because it races with
/// concurrent `getenv` in other threads. This is called as the first statement
/// of a test, before anything reads these variables, and the mandated runner
/// (`cargo nextest`) executes each test in its own process, so no sibling test
/// observes the mutation.
pub fn scrub_google_credential_env() -> Vec<String> {
    const CREDENTIAL_VARS: &[&str] = &[
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_CLOUD_QUOTA_PROJECT",
        "GOOGLE_MCP_QUOTA_PROJECT",
        "CLOUDSDK_CONFIG",
        "CLOUDSDK_CORE_PROJECT",
        "GCLOUD_PROJECT",
        "GOOGLE_GHA_CREDS_PATH",
    ];

    let mut removed = Vec::new();
    for name in CREDENTIAL_VARS {
        if std::env::var_os(name).is_some() {
            // SAFETY: see the function's "Safety and test-runner assumptions".
            unsafe { std::env::remove_var(name) };
            removed.push((*name).to_owned());
        }
        assert!(
            std::env::var_os(name).is_none(),
            "{name} must be absent for the credential-free assertions to mean anything"
        );
    }
    removed
}

/// A running in-process server plus the address it is listening on.
///
/// Dropping this cancels the accept loop, which is what the "upstream is down"
/// degradation test uses to take an endpoint offline deterministically.
pub struct TestServer {
    addr: SocketAddr,
    accept_loop: JoinHandle<()>,
}

impl TestServer {
    /// Socket this server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting connections and wait for the accept loop to finish.
    ///
    /// Explicit rather than relying on `Drop` so a test can assert on what
    /// happens *after* the upstream is provably gone, and so no accept task
    /// outlives the test (which nextest would flag as a leaked process).
    pub async fn shutdown(self) {
        self.accept_loop.abort();
        // `abort` is asynchronous; awaiting the handle makes the socket's
        // release ordered with respect to whatever the test does next.
        let _ = self.accept_loop.await;
    }
}

/// Serve `service` over TLS on an ephemeral loopback port.
///
/// `service` is cloned per connection. The certificate is self-signed for
/// `hostnames`; the test client skips verification, so the names matter only
/// for readability of failures.
async fn spawn_tls<S, B>(hostnames: Vec<String>, service: S) -> TestServer
where
    S: hyper::service::Service<Request<Incoming>, Response = Response<B>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: hyper::body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: std::error::Error + Send + Sync,
{
    install_crypto_provider();

    let certified = rcgen::generate_simple_self_signed(hostnames)
        .expect("generating a self-signed certificate for the in-process upstream");
    let cert = certified.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(certified.signing_key.serialize_der())
        .expect("the generated signing key is a valid PKCS#8 document");

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("the generated certificate and key form a valid pair");
    // reqwest negotiates via ALPN; offering both lets the client pick either
    // and keeps this harness agnostic to that choice.
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("binding an ephemeral loopback port");
    let addr = listener
        .local_addr()
        .expect("a bound listener reports its address");

    let accept_loop = tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                // The listener is gone; nothing further will arrive.
                return;
            };
            let acceptor = acceptor.clone();
            let service = service.clone();
            tokio::spawn(async move {
                let Ok(tls_stream) = acceptor.accept(stream).await else {
                    // A failed handshake is a client-side concern; the test
                    // asserting on the outcome will report it.
                    return;
                };
                let _ = ConnBuilder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(tls_stream), service)
                    .await;
            });
        }
    });

    TestServer { addr, accept_loop }
}

/// HTTP headers the upstream observed, in arrival order.
pub type ObservedHeaders = Arc<Mutex<Vec<HashMap<String, String>>>>;

/// Wraps a service, recording each request's headers before delegating.
///
/// Recording server-side is what makes the section 5.5 assertion meaningful:
/// the headers are read off the wire as the upstream received them, not
/// reported back by code under test.
#[derive(Clone)]
struct RecordHeaders<S> {
    inner: S,
    observed: ObservedHeaders,
}

impl<S, B> hyper::service::Service<Request<Incoming>> for RecordHeaders<S>
where
    S: hyper::service::Service<Request<Incoming>, Response = Response<B>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<B>;
    type Error = Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        let mut seen = HashMap::new();
        for (name, value) in request.headers() {
            seen.insert(
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            );
        }
        self.observed
            .lock()
            .expect("observed-header mutex is never held across a panic")
            .push(seen);
        let future = self.inner.call(request);
        Box::pin(future)
    }
}

/// Answers the first `reject_remaining` requests with a 401 challenge, then
/// forwards to `inner`.
///
/// Concretely typed to the boxed body rmcp's server produces so the rejection
/// branch and the forwarded branch share a response type.
#[derive(Clone)]
struct RejectInitializes<S> {
    inner: S,
    reject_remaining: Arc<AtomicUsize>,
}

impl<S> hyper::service::Service<Request<Incoming>> for RejectInitializes<S>
where
    S: hyper::service::Service<
            Request<Incoming>,
            Response = Response<BoxBody<Bytes, Infallible>>,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        // Decrement only while there are rejections left, so exactly the first
        // `reject` requests are refused and the rest pass through.
        let reject = self
            .reject_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if reject {
            return Box::pin(async {
                let body = r#"{"error":{"code":401,"message":"token revoked"}}"#;
                let response = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("content-type", "application/json")
                    // The challenge header is what makes rmcp classify this as
                    // authorization-required rather than an opaque failure.
                    .header("www-authenticate", "Bearer realm=\"google\"")
                    .body(BoxBody::new(Full::new(Bytes::from(body))))
                    .expect("a status and two headers always build a valid response");
                Ok(response)
            });
        }
        Box::pin(self.inner.call(request))
    }
}

/// A real MCP server with a small synthetic toolset.
///
/// Deliberately not a Google mock: it implements `ServerHandler` and is served
/// through rmcp's streamable-HTTP transport, so clients exercise the genuine
/// protocol against it.
#[derive(Clone)]
pub struct SyntheticUpstream {
    /// Headers observed by the transport, shared with the recording wrapper.
    observed: ObservedHeaders,
    /// Service name reported in `initialize`, to tell upstreams apart.
    label: String,
}

impl SyntheticUpstream {
    /// Tool declarations this upstream serves.
    fn tools() -> Vec<Tool> {
        let object_schema = |value: Value| -> Arc<JsonObject> {
            Arc::new(match value {
                Value::Object(map) => map,
                _ => JsonObject::new(),
            })
        };
        vec![
            Tool::new(
                TOOL_ECHO,
                "Echo the arguments back verbatim as JSON text.",
                object_schema(json!({
                    "type": "object",
                    "properties": { "payload": { "type": "string" } },
                })),
            ),
            Tool::new(
                TOOL_SHOW_HEADERS,
                "Report the HTTP headers this upstream received.",
                object_schema(json!({ "type": "object", "properties": {} })),
            ),
            Tool::new(
                TOOL_FAIL,
                "Always answer with an error result.",
                object_schema(json!({ "type": "object", "properties": {} })),
            ),
            Tool::new(
                TOOL_PROMOTES_HEADERS,
                "Report the received headers; its schema asks for an argument \
                 to be promoted into one.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "region": { "type": "string", "x-mcp-header": PROMOTED_HEADER },
                    },
                })),
            ),
        ]
    }
}

impl ServerHandler for SyntheticUpstream {
    fn get_info(&self) -> InitializeResult {
        let mut info = InitializeResult::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new(self.label.clone(), "0.0.0-test");
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: Self::tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let result = match request.name.as_ref() {
            TOOL_ECHO => {
                let arguments = request.arguments.unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string(&Value::Object(arguments))
                        .expect("a JSON object always serializes"),
                )])
            }
            TOOL_SHOW_HEADERS | TOOL_PROMOTES_HEADERS => {
                let latest = self
                    .observed
                    .lock()
                    .expect("observed-header mutex is never held across a panic")
                    .last()
                    .cloned()
                    .unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string(&latest).expect("a string map always serializes"),
                )])
            }
            TOOL_FAIL => CallToolResult::error(vec![ContentBlock::text(
                "synthetic upstream failure: this tool always fails",
            )]),
            other => CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown synthetic tool `{other}`"
            ))]),
        };
        Ok(CallToolResponse::Complete(result))
    }
}

/// A running synthetic MCP upstream and the headers it has observed.
pub struct McpUpstream {
    /// The listening server.
    pub server: TestServer,
    /// Headers seen per request, in arrival order.
    pub observed: ObservedHeaders,
}

impl McpUpstream {
    /// Auth-relevant headers from the most recent request carrying them.
    ///
    /// The transport issues several requests per session (initialize,
    /// notification, the call itself); every one should carry the headers, so
    /// this returns the last for assertion and [`Self::all_requests`] exposes
    /// the rest.
    pub fn last_headers(&self) -> HashMap<String, String> {
        self.observed
            .lock()
            .expect("observed-header mutex is never held across a panic")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// Every request's headers, in arrival order.
    pub fn all_requests(&self) -> Vec<HashMap<String, String>> {
        self.observed
            .lock()
            .expect("observed-header mutex is never held across a panic")
            .clone()
    }

    /// How many MCP `initialize` handshakes this upstream has served.
    ///
    /// The streamable-HTTP client has no session id until `initialize`
    /// completes, so the `initialize` POST is the one request that arrives
    /// without an `Mcp-Session-Id` header; every later request on that session
    /// carries it. Counting the header-less requests therefore counts
    /// handshakes -- including a failed one, whose 401 never yields a session
    /// id -- which is exactly what the dispatch-cache acceptance test asserts,
    /// and it needs no access to request bodies.
    pub fn initialize_count(&self) -> usize {
        self.all_requests()
            .iter()
            .filter(|headers| !headers.contains_key("mcp-session-id"))
            .count()
    }
}

/// Start a real MCP upstream over TLS for `hostnames`.
pub async fn spawn_mcp_upstream(hostnames: &[&str], label: &str) -> McpUpstream {
    spawn_mcp_upstream_rejecting(hostnames, label, 0).await
}

/// [`spawn_mcp_upstream`] that answers its first `reject` requests with a 401
/// carrying a `WWW-Authenticate: Bearer` challenge before serving normally.
///
/// This is how the dispatch cache's retry-once-on-401 is exercised: the first
/// `initialize` is rejected exactly as Google rejects a revoked token (a 401
/// with the challenge header, which is what rmcp turns into an
/// authorization-required error), and the proxy must re-fetch and open a
/// second session that succeeds. The rejection layer sits *inside* the header
/// recorder, so the rejected handshake still counts toward
/// [`McpUpstream::initialize_count`].
pub async fn spawn_mcp_upstream_rejecting(
    hostnames: &[&str],
    label: &str,
    reject: usize,
) -> McpUpstream {
    let observed: ObservedHeaders = Arc::new(Mutex::new(Vec::new()));

    let handler_observed = Arc::clone(&observed);
    let label = label.to_owned();

    // `StreamableHttpServerConfig` is #[non_exhaustive], so it is built by
    // mutation rather than a struct literal.
    let mut config = StreamableHttpServerConfig::default();
    // Responses come back as one JSON document instead of an SSE stream.
    // rmcp's client handles both; the simpler framing keeps failures readable.
    config.json_response = true;
    // The tests address this server as `run.googleapis.com` (via the DNS
    // override), so the default localhost-only Host allowlist would otherwise
    // reject every request.
    let config = config.disable_allowed_hosts();

    let service = StreamableHttpService::new(
        move || {
            Ok(SyntheticUpstream {
                observed: Arc::clone(&handler_observed),
                label: label.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let recording = RecordHeaders {
        inner: RejectInitializes {
            inner: TowerToHyperService::new(service),
            reject_remaining: Arc::new(AtomicUsize::new(reject)),
        },
        observed: Arc::clone(&observed),
    };
    let server = spawn_tls(
        hostnames.iter().map(|h| (*h).to_owned()).collect(),
        recording,
    )
    .await;

    McpUpstream { server, observed }
}

/// One page of a Service Usage `services.list` response.
///
/// Shaped exactly like the real API: entries carry `config.name`, and a
/// non-final page carries `nextPageToken`.
pub fn service_usage_page(api_names: &[&str], next_page_token: Option<&str>) -> String {
    let services: Vec<Value> = api_names
        .iter()
        .map(|api| {
            json!({
                "name": format!("projects/000000000000/services/{api}"),
                "config": { "name": api },
                "state": "ENABLED",
            })
        })
        .collect();
    let mut page = json!({ "services": services });
    if let Some(token) = next_page_token {
        page["nextPageToken"] = json!(token);
    }
    serde_json::to_string(&page).expect("the constructed page always serializes")
}

/// Start a Service Usage REST stub returning `pages` in order.
///
/// Plain HTTP JSON rather than MCP, which is what plan section 7 allows for
/// this one upstream. Pagination is real: each page but the last carries a
/// `nextPageToken`, and the stub hands back the page the token names, so
/// `enabled_services` must actually follow the chain to see every API.
pub async fn spawn_service_usage_stub(pages: Vec<String>) -> TestServer {
    spawn_counting_service_usage_stub(pages).await.0
}

/// [`spawn_service_usage_stub`] that also counts the requests it answers.
///
/// The counter is how a test proves a code path did *not* consult Service
/// Usage: `--only` promises to skip the enablement lookup, and a stub that
/// merely answers could not tell the difference between skipped and ignored.
pub async fn spawn_counting_service_usage_stub(
    pages: Vec<String>,
) -> (TestServer, Arc<AtomicUsize>) {
    let pages = Arc::new(pages);
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);

    let service = service_fn(move |request: Request<Incoming>| {
        let pages = Arc::clone(&pages);
        counter.fetch_add(1, Ordering::SeqCst);
        async move {
            let query = request.uri().query().unwrap_or_default().to_owned();
            // `pageToken=page-N` selects page N; its absence means the first.
            let index = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("pageToken="))
                .and_then(|token| token.rsplit("page-").next().and_then(|n| n.parse().ok()))
                .unwrap_or(0usize);

            let body = pages
                .get(index)
                .cloned()
                .unwrap_or_else(|| service_usage_page(&[], None));
            Ok::<_, Infallible>(json_response(StatusCode::OK, body))
        }
    });

    let server = spawn_tls(vec!["serviceusage.googleapis.com".to_owned()], service).await;
    (server, requests)
}

/// Start an upstream that answers every request with `status` and `body`.
///
/// Used to drive the error classifier end-to-end through the real dispatch
/// path, where the status survives only inside rmcp's rendered error text.
pub async fn spawn_failing_upstream(
    hostnames: &[&str],
    status: StatusCode,
    body: &'static str,
) -> TestServer {
    let service = service_fn(move |_request: Request<Incoming>| async move {
        Ok::<_, Infallible>(json_response(status, body.to_owned()))
    });
    spawn_tls(hostnames.iter().map(|h| (*h).to_owned()).collect(), service).await
}

/// Build a JSON response with the boxed body type hyper needs here.
fn json_response(status: StatusCode, body: String) -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(BoxBody::new(Full::new(Bytes::from(body))))
        .expect("a status and one header always build a valid response")
}

/// Build an HTTP client that resolves `hostnames` to `addr`.
///
/// This is the whole trick that lets the tests use production URLs: the code
/// under test still builds `https://run.googleapis.com/mcp`, and only DNS is
/// redirected. Certificate verification is disabled because the in-process
/// certificate is self-signed and generated fresh per run.
pub fn client_resolving(entries: &[(&str, SocketAddr)]) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent("mcp-google-service-integration-tests")
        .tls_danger_accept_invalid_certs(true);
    for (host, addr) in entries {
        builder = builder.resolve(host, *addr);
    }
    builder
        .build()
        .expect("the test client configuration is valid")
}
