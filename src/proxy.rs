//! Per-upstream MCP clients and request dispatch.
//!
//! An upstream MCP session is opened with an `initialize` handshake and then
//! carries the headers it was built with -- the bearer token among them --
//! for its whole life. Sessions are therefore cached per service and keyed by
//! the [`TokenGeneration`] their headers came from: a dispatch reuses the
//! session while the token it was built with is still the current one, and
//! rebuilds it the moment [`AuthContext`] reports a fresh token. ADC access
//! tokens expire hourly, so this is what keeps a long-lived session from
//! quietly outliving its credential.

use std::{collections::HashMap, sync::Arc, time::Duration};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, JsonObject},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::{sync::Mutex, time::Instant};

use crate::{
    auth::{AuthContext, TokenGeneration},
    catalog::split_namespaced,
    error::{classify_upstream, sanitize_body},
    registry::Endpoint,
};

/// Longest an upstream session may sit unused before the next dispatch
/// closes it.
///
/// Each live session holds its server-initiated event stream open, so an idle
/// one is an open connection on both sides; five minutes keeps a working
/// model's sessions warm and lets an abandoned one go.
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(5 * 60);

/// Most upstream sessions kept alive at once; the least recently used is
/// closed first.
///
/// A typical project enables a handful of the registered APIs, so this is
/// headroom, not a working limit; it exists so that 47 idle sessions cannot
/// accumulate on a server that touched every service once.
pub const MAX_SESSIONS: usize = 16;

/// Where one service's MCP endpoint lives.
///
/// Routes are injected rather than read from the static registry so that tests
/// can point dispatch at an in-process upstream on `127.0.0.1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Service id used as the tool-name prefix.
    pub service_id: String,
    /// Host reported in error messages.
    pub host: String,
    /// Full MCP URL to POST to.
    pub mcp_url: String,
}

impl Route {
    /// Route derived from a registry endpoint.
    pub fn from_endpoint(endpoint: &Endpoint) -> Self {
        Self {
            service_id: endpoint.service_id.to_owned(),
            host: endpoint.host.to_owned(),
            mcp_url: endpoint.mcp_url(),
        }
    }
}

/// One live upstream session and what it was built from.
struct CachedSession {
    /// Token generation its headers carry; a newer one means rebuild.
    generation: TokenGeneration,
    /// The session. Shared so a dispatch in flight keeps it alive after the
    /// cache has let go of it; the last holder's drop closes it.
    service: Arc<RunningService<RoleClient, ()>>,
    /// When a dispatch last used it, for idle eviction and LRU ordering.
    last_used: Instant,
}

/// Dispatches namespaced tool calls to the upstream that owns them.
pub struct Proxy {
    auth: Arc<AuthContext>,
    http: reqwest::Client,
    routes: Vec<Route>,
    /// Live sessions by service id. The lock is held only to look up, insert
    /// or evict, never across a handshake or a call.
    sessions: Mutex<HashMap<String, CachedSession>>,
    idle_ttl: Duration,
    max_sessions: usize,
}

impl Proxy {
    /// Build a proxy over a shared HTTP client and an explicit route table.
    ///
    /// The client is shared across every upstream on purpose: rmcp's
    /// `from_uri` constructor builds a fresh `reqwest::Client` per transport,
    /// which pays TLS setup once per host per call. One pooled client amortizes
    /// connection setup across the whole process.
    pub fn new(auth: Arc<AuthContext>, http: reqwest::Client, routes: Vec<Route>) -> Self {
        debug_assert!(
            routes.iter().all(|r| r.host.ends_with(".googleapis.com")),
            "dispatch attaches a bearer token; routes must never leave googleapis.com"
        );
        Self {
            auth,
            http,
            routes,
            sessions: Mutex::default(),
            idle_ttl: SESSION_IDLE_TTL,
            max_sessions: MAX_SESSIONS,
        }
    }

    /// Override the session cache's idle timeout and size bound.
    ///
    /// The defaults ([`SESSION_IDLE_TTL`], [`MAX_SESSIONS`]) are what serve
    /// uses; this exists so the eviction rules can be exercised by tests
    /// without waiting minutes or opening seventeen upstreams.
    pub fn with_session_limits(mut self, idle_ttl: Duration, max_sessions: usize) -> Self {
        self.idle_ttl = idle_ttl;
        self.max_sessions = max_sessions.max(1);
        self
    }

    /// Number of upstream sessions currently cached.
    pub async fn cached_sessions(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Build a proxy routing to the given registry endpoints.
    pub fn from_endpoints(
        auth: Arc<AuthContext>,
        http: reqwest::Client,
        endpoints: &[&Endpoint],
    ) -> Self {
        Self::new(
            auth,
            http,
            endpoints.iter().map(|e| Route::from_endpoint(e)).collect(),
        )
    }

    /// Resolve a service prefix to its route.
    fn route(&self, service_id: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.service_id == service_id)
    }

    /// Message naming the services this proxy can actually reach.
    fn unknown_service_message(&self, service_id: &str) -> String {
        let mut known: Vec<&str> = self.routes.iter().map(|r| r.service_id.as_str()).collect();
        known.sort_unstable();
        format!(
            "unknown service `{service_id}`; no exposed Google MCP endpoint uses that \
             prefix. Exposed services: {}.",
            known.join(", ")
        )
    }

    /// Call `namespaced_name` on its upstream and return the result verbatim.
    ///
    /// Never returns `Err`: routing problems and upstream failures are rendered
    /// as `isError` results carrying remediation text, because the caller is a
    /// model that can act on the message but not on a protocol error.
    pub async fn dispatch(
        &self,
        namespaced_name: &str,
        arguments: Option<JsonObject>,
    ) -> CallToolResult {
        match self.try_dispatch(namespaced_name, arguments).await {
            Ok(result) => result,
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        }
    }

    /// [`Self::dispatch`] with failures as `Err(message)`.
    ///
    /// The upstream session is taken from the cache when one exists for the
    /// service and was built from the token that is current now; otherwise a
    /// new one is opened with one `initialize` handshake and cached.
    ///
    /// A stale token is handled two ways that never double-execute a tool.
    /// The ordinary rotation -- the local token expiring and being refreshed --
    /// changes the [`TokenGeneration`], so the old-generation session is not
    /// reused and the next dispatch simply rebuilds with the fresh token. The
    /// awkward case is a token that still looks current locally but the server
    /// rejects (revoked, or a clock skew the expiry check missed). If that 401
    /// arrives while *opening* a session it is detected
    /// ([`ClientInitializeError::is_authorization_required`]) and the open is
    /// retried once against a re-fetched token, because a rejected handshake
    /// ran no tool. If it instead arrives on a *reused* session's call, that
    /// call is not silently replayed -- the session is dropped and the error
    /// returned, and the model's next call rebuilds the session, where the
    /// same 401 is caught at the handshake and recovered. Every non-auth call
    /// failure likewise drops the session and is returned as is.
    async fn try_dispatch(
        &self,
        namespaced_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, String> {
        let (service_id, tool_name) = split_namespaced(namespaced_name)
            .ok_or_else(|| unnamespaced_message(namespaced_name))?;
        let route = self
            .route(service_id)
            .ok_or_else(|| self.unknown_service_message(service_id))?;

        let session = self.session_for(route, tool_name).await?;

        let mut params = CallToolRequestParams::new(tool_name.to_owned());
        params.arguments = arguments;
        match session.call_tool(params).await {
            Ok(result) => Ok(result),
            Err(error) => {
                // A reused session may have gone stale under us, so drop it and
                // let the next dispatch rebuild.
                //
                // This call is deliberately NOT retried, and that is a safety
                // property rather than caution. A failure here can arrive
                // *after* the upstream has already run the tool -- a response
                // lost on the way back looks exactly like a request that never
                // arrived -- and the tools reached through this proxy are not
                // all idempotent: `run__deploy_service_from_image`,
                // `compute__create_instance` and `compute__delete_instance` are
                // among them. Retrying would risk deploying or creating twice.
                //
                // The stale-token case that a retry would have covered is
                // recovered anyway, one dispatch later: the session is gone, so
                // the next call opens a new one, and there the rejection
                // arrives at the handshake, where it is positively identifiable
                // and no tool has run yet (see `session_for`). Anyone extending
                // this into a general retry must keep that split -- retry only
                // where the upstream refused *before* executing.
                self.evict(&route.service_id, &session).await;
                Err(upstream_message(&route.host, &error))
            }
        }
    }

    /// A live session for `route`: the cached one when it is current and open,
    /// otherwise a freshly opened one. A handshake the server rejects as
    /// unauthorized is retried once against a re-fetched token.
    ///
    /// `tool_name` is only for the log line that attributes a handshake to the
    /// call that caused it.
    async fn session_for(
        &self,
        route: &Route,
        tool_name: &str,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, String> {
        let mut retried = false;
        loop {
            let (headers, generation) = match self.call_headers().await {
                Ok(fresh) => fresh,
                Err(message) => {
                    // A session exists only while the credential it carries is
                    // current; with none obtainable, none may linger.
                    self.sessions.lock().await.clear();
                    return Err(message);
                }
            };

            match self
                .open_or_reuse(route, generation, headers, tool_name)
                .await
            {
                Ok(session) => return Ok(session),
                Err(error) => {
                    if !retried && error.is_authorization_required() {
                        tracing::debug!(
                            service = route.service_id,
                            "upstream rejected the token at handshake; re-fetching and retrying once"
                        );
                        retried = true;
                        self.auth.invalidate(generation).await;
                        continue;
                    }
                    return Err(upstream_message(&route.host, &error));
                }
            }
        }
    }

    /// The cached session for `route` if it was built from `generation` and
    /// its transport is still open; otherwise a freshly initialized one, which
    /// replaces whatever was cached.
    ///
    /// The handshake runs outside the cache lock. Two first dispatches to the
    /// same service racing each other may therefore both open a session; the
    /// later insert wins the cache and the other closes once its own call is
    /// done, which costs one redundant handshake and nothing else.
    async fn open_or_reuse(
        &self,
        route: &Route,
        generation: TokenGeneration,
        headers: HashMap<HeaderName, HeaderValue>,
        tool_name: &str,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, rmcp::service::ClientInitializeError> {
        {
            let mut sessions = self.sessions.lock().await;
            self.evict_idle(&mut sessions);
            if let Some(cached) = sessions.get_mut(&route.service_id)
                && cached.generation == generation
                && !cached.service.is_transport_closed()
            {
                cached.last_used = Instant::now();
                return Ok(Arc::clone(&cached.service));
            }
        }

        let transport = StreamableHttpClientTransport::with_client(
            self.http.clone(),
            StreamableHttpClientTransportConfig::with_uri(route.mcp_url.clone())
                .custom_headers(headers),
        );
        let service = Arc::new(().serve(transport).await?);
        tracing::debug!(
            service = route.service_id,
            host = route.host,
            tool = tool_name,
            "opened upstream MCP session"
        );

        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            route.service_id.clone(),
            CachedSession {
                generation,
                service: Arc::clone(&service),
                last_used: Instant::now(),
            },
        );
        self.enforce_bound(&mut sessions);
        Ok(service)
    }

    /// Drop `session` from the cache if it is still the one cached for
    /// `service_id`; a newer replacement is left alone.
    async fn evict(&self, service_id: &str, session: &Arc<RunningService<RoleClient, ()>>) {
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(service_id)
            .is_some_and(|cached| Arc::ptr_eq(&cached.service, session))
        {
            sessions.remove(service_id);
        }
    }

    /// Close sessions that have sat unused longer than the idle timeout.
    fn evict_idle(&self, sessions: &mut HashMap<String, CachedSession>) {
        let now = Instant::now();
        sessions.retain(|service_id, cached| {
            let keep = now.duration_since(cached.last_used) < self.idle_ttl;
            if !keep {
                tracing::debug!(service = %service_id, "closing idle upstream MCP session");
            }
            keep
        });
    }

    /// Close least-recently-used sessions until the cache is within bound.
    fn enforce_bound(&self, sessions: &mut HashMap<String, CachedSession>) {
        while sessions.len() > self.max_sessions {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(service_id, _)| service_id.clone())
            else {
                return;
            };
            tracing::debug!(service = %oldest, "closing least recently used upstream MCP session");
            sessions.remove(&oldest);
        }
    }

    /// Freshly minted auth headers for one request, and the token generation
    /// they carry.
    ///
    /// `Authorization` and `x-goog-user-project` both travel as custom headers;
    /// neither is on rmcp's reserved list, and routing the bearer token this way
    /// keeps both headers on the same call-time refresh path.
    async fn call_headers(
        &self,
    ) -> Result<(HashMap<HeaderName, HeaderValue>, TokenGeneration), String> {
        let mut headers = HeaderMap::new();
        let generation = self
            .auth
            .apply_tracked(&mut headers)
            .await
            // Sanitized like the sibling upstream path: this renders into the
            // same model-visible sink, and while gcp_auth 0.12.7's variants
            // all lead with crate-controlled static text, that is a property
            // of a pinned version rather than an invariant a minor bump must
            // preserve.
            .map_err(|error| {
                format!(
                    "could not attach Google credentials: {}",
                    sanitize_body(&error.to_string())
                )
            })?;
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        Ok((headers, generation))
    }
}

/// The HTTP status behind an rmcp error, typed where one is carried and
/// recovered from the rendered chain otherwise (see [`status_from_text`]).
fn recovered_status(error: &(dyn std::error::Error + 'static)) -> Option<u16> {
    http_status(error).or_else(|| status_from_text(&render_chain(error)))
}

/// Build the shared HTTP client used for both dispatch and Service Usage calls.
///
/// Every URL this client is ever asked for is built by this crate against a
/// fixed `*.googleapis.com` host over TLS, which makes the restrictions below
/// free of behavioural cost and worth having:
///
/// * **No redirects.** Nothing in Google's MCP or Service Usage surface
///   redirects, so a 30x can only be an upstream (or an interposer) trying to
///   move a request that carries a live access token somewhere else. Following
///   it would forward the `Authorization` header along with it.
/// * **HTTPS only.** A downgrade to plaintext would put the same token on the
///   wire in the clear; refusing the scheme outright is stronger than trusting
///   every future call site to spell `https`.
/// * **Bounded connect and total time.** Without them a single unresponsive
///   host holds a task, and its permit, indefinitely.
pub fn shared_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(concat!("mcp-google-service/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
}

/// Message for a tool name that carries no `{service}__{tool}` prefix.
fn unnamespaced_message(name: &str) -> String {
    format!(
        "`{name}` is not a namespaced tool name; expected `{{service}}__{{tool}}`, \
         for example `run__list_services`. Use `list_services` to see the \
         available service prefixes."
    )
}

/// Render an upstream failure with the remediation its classification carries.
///
/// rmcp surfaces HTTP failures as a nested error chain rather than a status and
/// body, so the status is recovered by walking the chain for the underlying
/// [`reqwest::Error`] and the rendered chain stands in for the response body.
/// The rendered chain embeds the upstream's response body, so the unclassified
/// branch passes it through [`sanitize_body`] before it becomes model-visible
/// text: classification is matched on the raw chain, but nothing raw is
/// rendered.
fn upstream_message(host: &str, error: &(dyn std::error::Error + 'static)) -> String {
    let chain = render_chain(error);
    match recovered_status(error) {
        Some(status) => format!(
            "call to {host} failed: {}",
            classify_upstream(status, &chain)
        ),
        None => format!("call to {host} failed: {}", sanitize_body(&chain)),
    }
}

/// Recover an HTTP status that survived only as text.
///
/// rmcp reports a non-2xx response that is not an OAuth challenge as
/// `UnexpectedServerResponse("HTTP 403 Forbidden: {body}")` rather than a typed
/// error, so neither the status nor the body is reachable through
/// [`reqwest::Error`]. Both are still present in the rendered chain, which is
/// what lets Google's body-discriminated failures (`SERVICE_DISABLED`, quota
/// project, API-key rejection) be classified at all.
fn status_from_text(text: &str) -> Option<u16> {
    text.match_indices("HTTP ").find_map(|(index, marker)| {
        let digits: String = text[index + marker.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits
            .parse()
            .ok()
            .filter(|code| (100..=599).contains(code))
    })
}

/// Find the HTTP status carried by any [`reqwest::Error`] in the chain.
fn http_status(error: &(dyn std::error::Error + 'static)) -> Option<u16> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(status) = error
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
        {
            return Some(status.as_u16());
        }
        current = error.source();
    }
    None
}

/// Join an error and its sources into one line.
fn render_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut current = error.source();
    while let Some(source) = current {
        let rendered = source.to_string();
        if !parts.contains(&rendered) {
            parts.push(rendered);
        }
        current = source.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    #[test]
    fn unnamespaced_names_are_rejected_with_guidance() {
        let message = unnamespaced_message("list_services");
        assert!(message.contains("list_services"));
        assert!(message.contains("{service}__{tool}"));
    }

    /// Every route the production constructor can produce stays on Google.
    ///
    /// This is the strongest invariant in the binary and until now it held by
    /// construction alone: dispatch attaches a bearer token to whatever host
    /// the route names, and nothing failed if a refactor pointed one
    /// elsewhere. `Route`'s fields are public and `Proxy::new` is public, so
    /// "we only ever call `from_endpoints`" was a convention rather than a
    /// check. Asserting it over the whole registry costs nothing and turns a
    /// silent refactor into a red test.
    #[test]
    fn every_route_from_the_registry_stays_on_googleapis_com() {
        let endpoints: Vec<&Endpoint> = registry::ENDPOINTS.iter().collect();
        assert!(
            !endpoints.is_empty(),
            "an empty registry would make this assertion vacuous"
        );

        let proxy = Proxy::from_endpoints(
            Arc::new(
                crate::auth::AuthContext::with_source(Arc::new(UnusedTokenSource), "test-project")
                    .expect("a literal project id is a valid header value"),
            ),
            reqwest::Client::new(),
            &endpoints,
        );

        for route in &proxy.routes {
            assert!(
                route.host.ends_with(".googleapis.com"),
                "route `{}` leaves Google at host `{}`; dispatch would attach a \
                 bearer token to it",
                route.service_id,
                route.host
            );
            assert!(
                route.mcp_url.starts_with("https://")
                    && route.mcp_url.ends_with(".googleapis.com/mcp"),
                "route `{}` has a URL dispatch should never post a token to: {}",
                route.service_id,
                route.mcp_url
            );
        }
    }

    /// The guard itself bites, not just the registry that satisfies it.
    ///
    /// Without this, the `debug_assert` in [`Proxy::new`] is a line nothing
    /// executes: every route the tests build is already on Google, so the
    /// assertion passes vacuously and a future edit could weaken it unnoticed.
    /// `cfg(debug_assertions)` so the test exists exactly when the assert
    /// does -- `debug_assert` compiles out under `--release`, and a
    /// `should_panic` test that outlived it would fail a release test run.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "routes must never leave googleapis.com")]
    fn a_route_off_google_trips_the_guard() {
        let auth = Arc::new(
            crate::auth::AuthContext::with_source(Arc::new(UnusedTokenSource), "test-project")
                .expect("a literal project id is a valid header value"),
        );
        let _ = Proxy::new(
            auth,
            reqwest::Client::new(),
            vec![route_to_base_url("evil", "https://attacker.example")],
        );
    }

    /// A token source that must never be consulted; this test never dispatches.
    struct UnusedTokenSource;

    impl crate::auth::TokenSource for UnusedTokenSource {
        fn fetch(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::auth::FetchedToken, crate::error::Error>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { panic!("the route invariant test never dispatches") })
        }
    }

    #[test]
    fn routes_are_built_from_registry_endpoints() {
        let endpoint = registry::find("run").expect("run is registered");
        let route = Route::from_endpoint(endpoint);
        assert_eq!(route.service_id, "run");
        assert_eq!(route.host, "run.googleapis.com");
        assert_eq!(route.mcp_url, "https://run.googleapis.com/mcp");
    }

    /// Route to an arbitrary base URL.
    ///
    /// Test-only, and deliberately not part of the crate's API: production
    /// routes come from the registry, and the integration tier reaches its
    /// in-process upstreams by overriding DNS for the real hostnames rather
    /// than by rewriting URLs. A public constructor for arbitrary URLs would
    /// be a way to point authenticated dispatch, bearer token included, at a
    /// host Google does not own.
    fn route_to_base_url(service_id: &str, base_url: &str) -> Route {
        Route {
            service_id: service_id.to_owned(),
            host: base_url.to_owned(),
            mcp_url: format!("{}/mcp", base_url.trim_end_matches('/')),
        }
    }

    #[test]
    fn routes_can_target_an_arbitrary_base_url() {
        let route = route_to_base_url("run", "http://127.0.0.1:8931");
        assert_eq!(route.mcp_url, "http://127.0.0.1:8931/mcp");
        // A trailing slash must not produce a doubled separator.
        assert_eq!(
            route_to_base_url("run", "http://127.0.0.1:8931/").mcp_url,
            "http://127.0.0.1:8931/mcp"
        );
    }

    #[test]
    fn an_unclassified_upstream_failure_is_bounded_and_control_free() {
        #[derive(Debug)]
        struct Noisy(String);
        impl std::fmt::Display for Noisy {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Noisy {}

        // No status anywhere, so this takes the passthrough branch: the one
        // place an upstream's own bytes reach the caller unclassified.
        let noisy = Noisy(format!("\u{1b}[2Jcleared\r{}", "B".repeat(64 * 1024)));
        let message = upstream_message("run.googleapis.com", &noisy);

        assert!(message.starts_with("call to run.googleapis.com failed: "));
        assert!(!message.contains('\u{1b}'), "an escape reached the caller");
        assert!(
            !message.contains('\r'),
            "a carriage return reached the caller"
        );
        assert!(
            message.len() < 8 * 1024,
            "an unbounded upstream body reached the caller: {} bytes",
            message.len()
        );
    }

    #[test]
    fn dispatch_targets_split_on_the_first_separator() {
        // Upstream tool names may themselves contain `__`; only the first
        // separator delimits the service prefix.
        assert_eq!(
            split_namespaced("run__list_services"),
            Some(("run", "list_services"))
        );
        assert_eq!(
            split_namespaced("run__weird__name"),
            Some(("run", "weird__name"))
        );
        assert!(split_namespaced("run").is_none());
    }

    #[test]
    fn every_registered_service_id_survives_a_round_trip() {
        // Dispatch routing relies on no service id containing the separator.
        for endpoint in registry::ENDPOINTS {
            let namespaced = format!("{}__tool", endpoint.service_id);
            assert_eq!(
                split_namespaced(&namespaced),
                Some((endpoint.service_id, "tool"))
            );
            assert!(registry::find(endpoint.service_id).is_some());
        }
    }

    #[test]
    fn http_status_is_recovered_from_a_nested_chain() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "outer")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let wrapped = Wrapper(std::io::Error::other("inner"));
        // No reqwest::Error anywhere in this chain, so no status is available.
        assert_eq!(http_status(&wrapped), None);
        assert_eq!(render_chain(&wrapped), "outer: inner");
    }

    #[test]
    fn status_is_recovered_from_rmcp_text_when_no_typed_error_exists() {
        // Verbatim shape observed from run.googleapis.com: rmcp folds the
        // status and body into UnexpectedServerResponse's message.
        let observed = "Transport send error: unexpected server response: \
             HTTP 403 Forbidden: {\"error\":{\"message\":\"Permission denied\"}}";
        assert_eq!(status_from_text(observed), Some(403));

        assert_eq!(status_from_text("HTTP 401 Unauthorized"), Some(401));
        assert_eq!(status_from_text("HTTP 500"), Some(500));
        assert_eq!(status_from_text("no status here"), None);
        // A bare "HTTP " with no code must not be read as a status.
        assert_eq!(status_from_text("HTTP Forbidden"), None);
        // Out-of-range numbers are not statuses.
        assert_eq!(status_from_text("HTTP 9999"), None);
        // Scanning continues past a non-status occurrence.
        assert_eq!(
            status_from_text("HTTP/1.1 -- HTTP 404 Not Found"),
            Some(404)
        );
    }

    #[test]
    fn render_chain_does_not_repeat_identical_layers() {
        #[derive(Debug)]
        struct Echo(Option<Box<Echo>>);
        impl std::fmt::Display for Echo {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "same")
            }
        }
        impl std::error::Error for Echo {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.0
                    .as_deref()
                    .map(|e| e as &(dyn std::error::Error + 'static))
            }
        }

        let nested = Echo(Some(Box::new(Echo(None))));
        assert_eq!(render_chain(&nested), "same");
    }
}
