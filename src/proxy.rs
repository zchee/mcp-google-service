//! Per-upstream MCP clients and request dispatch.
//!
//! Every dispatch builds its transport with headers taken from
//! [`AuthContext`] at call time. ADC access tokens expire hourly, so a header
//! captured once at construction would go stale; the only safe place to read
//! the token is immediately before the request that uses it.

use std::{collections::HashMap, sync::Arc};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, JsonObject},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

use crate::{
    auth::AuthContext,
    catalog::split_namespaced,
    error::{classify_upstream, sanitize_body},
    registry::Endpoint,
};

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

/// Dispatches namespaced tool calls to the upstream that owns them.
pub struct Proxy {
    auth: Arc<AuthContext>,
    http: reqwest::Client,
    routes: Vec<Route>,
}

impl Proxy {
    /// Build a proxy over a shared HTTP client and an explicit route table.
    ///
    /// The client is shared across every upstream on purpose: rmcp's
    /// `from_uri` constructor builds a fresh `reqwest::Client` per transport,
    /// which pays TLS setup once per host per call. One pooled client amortizes
    /// connection setup across the whole process.
    pub fn new(auth: Arc<AuthContext>, http: reqwest::Client, routes: Vec<Route>) -> Self {
        Self { auth, http, routes }
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

        let transport = StreamableHttpClientTransport::with_client(
            self.http.clone(),
            StreamableHttpClientTransportConfig::with_uri(route.mcp_url.clone())
                .custom_headers(self.call_headers().await?),
        );

        let client =
            ().serve(transport)
                .await
                .map_err(|error| upstream_message(&route.host, &error))?;

        let mut params = CallToolRequestParams::new(tool_name.to_owned());
        params.arguments = arguments;
        let called = client.call_tool(params).await;
        // Release the upstream session regardless of outcome.
        let _ = client.cancel().await;

        called.map_err(|error| upstream_message(&route.host, &error))
    }

    /// Freshly minted auth headers for one request.
    ///
    /// `Authorization` and `x-goog-user-project` both travel as custom headers;
    /// neither is on rmcp's reserved list, and routing the bearer token this way
    /// keeps both headers on the same call-time refresh path.
    async fn call_headers(&self) -> Result<HashMap<HeaderName, HeaderValue>, String> {
        let mut headers = HeaderMap::new();
        self.auth
            .apply(&mut headers)
            .await
            .map_err(|error| format!("could not attach Google credentials: {error}"))?;
        Ok(headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect())
    }
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
    match http_status(error).or_else(|| status_from_text(&chain)) {
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
