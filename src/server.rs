//! The stdio-facing MCP server surface.
//!
//! Two shapes are served from one implementation. The default two-tier surface
//! exposes four meta-tools so the model sees a handful of tools instead of
//! hundreds, loading schemas on demand; `--expose flat` registers every pruned
//! namespaced tool with its real schema. Dispatch is identical in both.

use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ServerCapabilities, Tool,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::{Value, json};
use tokio::sync::{RwLock, watch};

use crate::{
    auth::AuthContext,
    catalog::{Catalog, CatalogError, CatalogSource},
    config::{Config, ExposeMode},
    proxy::Proxy,
    prune,
    registry::{self, Endpoint},
};

/// Meta-tool: enumerate the exposed services.
const TOOL_LIST_SERVICES: &str = "list_services";
/// Meta-tool: rank tools by keyword.
const TOOL_SEARCH_TOOLS: &str = "search_tools";
/// Meta-tool: fetch full schemas for named tools.
const TOOL_DESCRIBE_TOOLS: &str = "describe_tools";
/// Meta-tool: dispatch a namespaced tool call upstream.
const TOOL_CALL: &str = "call";

/// Default number of search hits returned when the caller does not say.
const DEFAULT_SEARCH_LIMIT: usize = 20;

/// A catalog that can be replaced while the server is running.
///
/// `serve` starts from the snapshot so the first tool call is answerable
/// immediately, then swaps in the live catalog when the background refresh
/// lands. In two-tier mode the four exposed tools never change, so the swap
/// needs no `listChanged` notification.
pub type SharedCatalog = Arc<RwLock<Arc<Catalog>>>;

/// The two catalog views one server serves from.
///
/// `live` is what the background refresh replaces and what the two-tier
/// meta-tools read, so `list_services`, `search_tools` and `describe_tools`
/// answer from the freshest data available.
///
/// `startup` is frozen when the server is assembled and is what `--expose
/// flat` serves. Flat mode hands the client a concrete list of tools at
/// `initialize`, and this server sends no `listChanged` notification (there is
/// nothing to notify with, by design: the two-tier surface never changes). A
/// refresh that altered the flat list would therefore leave the client holding
/// a tool list the server no longer agrees with -- offering tools that have
/// silently vanished, and hiding tools it will accept. Freezing it makes flat
/// mode's surface exactly as stable as the client's copy of it.
#[derive(Clone)]
pub struct CatalogState {
    live: SharedCatalog,
    startup: Arc<Catalog>,
    readiness: watch::Sender<Readiness>,
}

impl CatalogState {
    /// Freeze `catalog` as the startup surface and start serving it live.
    ///
    /// Readiness starts as [`Readiness::pending`]: nothing has been resolved
    /// on the caller's behalf until something publishes otherwise.
    pub fn new(catalog: Catalog) -> Self {
        let startup = Arc::new(catalog);
        let (readiness, _) = watch::channel(Readiness::pending());
        Self {
            live: Arc::new(RwLock::new(Arc::clone(&startup))),
            startup,
            readiness,
        }
    }

    /// Handle the background refresh writes its result into.
    pub fn live(&self) -> SharedCatalog {
        Arc::clone(&self.live)
    }

    /// The catalog as it stood when the server was assembled.
    pub fn startup(&self) -> &Arc<Catalog> {
        &self.startup
    }

    /// The catalog as it stands now.
    async fn current(&self) -> Arc<Catalog> {
        Arc::clone(&*self.live.read().await)
    }

    /// The most recently published startup readiness.
    pub fn readiness(&self) -> Readiness {
        self.readiness.borrow().clone()
    }

    /// Publish a readiness transition to every reader.
    pub fn publish_readiness(&self, readiness: Readiness) {
        self.readiness.send_replace(readiness);
    }

    /// A receiver that observes readiness transitions as they are published.
    pub fn readiness_watch(&self) -> watch::Receiver<Readiness> {
        self.readiness.subscribe()
    }
}

/// Whether the process holds usable Google credentials yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialState {
    /// Not acquired yet: the background startup has not reached them, and no
    /// `call` has forced them.
    Pending,
    /// A token was acquired and is cached.
    Ready,
    /// Acquisition failed; the text is what every `call` returns until it
    /// succeeds on a later attempt.
    Failed(String),
}

impl CredentialState {
    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Failed(_) => "failed",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason),
            Self::Pending | Self::Ready => None,
        }
    }
}

/// How the exposed set relates to what Service Usage reports as enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnablementState {
    /// Service Usage has not been consulted yet; every configured service is
    /// exposed meanwhile, and a call to a disabled one fails upstream with the
    /// classified `SERVICE_DISABLED` remediation.
    Pending,
    /// Narrowed to the APIs Service Usage reports enabled.
    Pruned,
    /// `--only` named the services, so Service Usage was deliberately not
    /// asked.
    Skipped,
    /// Service Usage could not be consulted; the configured selection is
    /// exposed unpruned and the text says why.
    Unknown(String),
}

impl EnablementState {
    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Pruned => "pruned",
            Self::Skipped => "skipped",
            Self::Unknown(_) => "unknown",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Unknown(reason) => Some(reason),
            Self::Pending | Self::Pruned | Self::Skipped => None,
        }
    }
}

/// Startup progress, reported through `list_services`.
///
/// `serve` answers `initialize` and `tools/list` before it has credentials or
/// knows which APIs are enabled; this is how a caller tells "not yet" from
/// "broken" without reading the server's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    /// Credential acquisition.
    pub credentials: CredentialState,
    /// Enablement pruning.
    pub enablement: EnablementState,
}

impl Readiness {
    /// Nothing resolved yet.
    pub fn pending() -> Self {
        Self {
            credentials: CredentialState::Pending,
            enablement: EnablementState::Pending,
        }
    }

    /// Readiness of any one exposed service, as the label `list_services`
    /// shows next to it.
    ///
    /// `failed` means no call can succeed until credentials are repaired;
    /// `pending` means the answer is not in yet; `unverified` means the
    /// credentials are good but the API's enablement could not be checked, so
    /// a call may still fail upstream with `SERVICE_DISABLED`; `ready` means
    /// both are settled.
    pub fn service_label(&self) -> &'static str {
        match (&self.credentials, &self.enablement) {
            (CredentialState::Failed(_), _) => "failed",
            (CredentialState::Pending, _) | (_, EnablementState::Pending) => "pending",
            (CredentialState::Ready, EnablementState::Unknown(_)) => "unverified",
            (CredentialState::Ready, EnablementState::Pruned | EnablementState::Skipped) => "ready",
        }
    }
}

/// Build the catalog state the serve path starts from.
///
/// Three steps that belong together, extracted from the binary so they can be
/// asserted on: narrow the snapshot to the endpoints actually exposed, relabel
/// every service as [`CatalogSource::Snapshot`], and freeze the result as the
/// startup surface.
///
/// The relabel is the load-bearing step. A snapshot file records the
/// provenance its *capture run* saw, which is `live` for every service that
/// answered at the time. A process replaying those bytes has not talked to
/// anything yet, so repeating that claim would report hour-old or month-old
/// tool definitions as freshly fetched, and `list_services` would tell an
/// operator the opposite of the truth.
///
/// # Errors
///
/// Per [`Snapshot::into_catalog`]: the snapshot must satisfy the namespacing
/// invariants.
pub fn assemble_serve_catalog(
    catalog: Catalog,
    exposed: &[&Endpoint],
) -> Result<CatalogState, CatalogError> {
    Ok(CatalogState::new(
        catalog
            .restricted_to(exposed)
            .marked_as(CatalogSource::Snapshot),
    ))
}

/// The endpoints the configuration admits before enablement is known:
/// the registry narrowed by `--only` and `--exclude`.
pub fn configured_endpoints(cfg: &Config) -> Vec<&'static Endpoint> {
    prune::select_services(registry::ENDPOINTS, None, &cfg.only, &cfg.exclude)
}

/// Which endpoints to expose, and how that was decided.
pub struct Exposure {
    /// Endpoints to expose, in registry order.
    pub endpoints: Vec<&'static Endpoint>,
    /// Whether Service Usage was consulted, skipped, or could not answer.
    pub enablement: EnablementState,
}

/// Decide which endpoints to expose, consulting Service Usage only when the
/// configuration has not already pinned the set.
///
/// `--only` names the services outright, so Service Usage is not asked: the
/// answer could not change the selection, and the call costs a token fetch
/// plus a round trip that was measured at ~1.5 s. A Service Usage failure is
/// never fatal: it warns, names the cause, and exposes the configured
/// selection unpruned (plan P3 fallback policy), which the returned
/// [`EnablementState::Unknown`] makes visible to callers.
pub async fn resolve_enablement(
    cfg: &Config,
    auth: &AuthContext,
    http: &reqwest::Client,
) -> Exposure {
    if !cfg.only.is_empty() {
        let endpoints = configured_endpoints(cfg);
        tracing::info!(
            services = endpoints.len(),
            of = registry::ENDPOINTS.len(),
            "`--only` pins the exposed services; Service Usage not consulted"
        );
        return Exposure {
            endpoints,
            enablement: EnablementState::Skipped,
        };
    }

    match prune::enabled_services(auth, &cfg.quota_project, http).await {
        Ok(enabled) => {
            tracing::info!(
                enabled = enabled.len(),
                project = %cfg.quota_project,
                "Service Usage reported enabled APIs"
            );
            let endpoints = prune::select_services(
                registry::ENDPOINTS,
                Some(&enabled),
                &cfg.only,
                &cfg.exclude,
            );
            tracing::info!(
                services = endpoints.len(),
                of = registry::ENDPOINTS.len(),
                "services selected for exposure"
            );
            Exposure {
                endpoints,
                enablement: EnablementState::Pruned,
            }
        }
        Err(error) => {
            tracing::warn!(
                project = %cfg.quota_project,
                cause = %error,
                "could not determine enabled APIs; exposing the configured selection unpruned"
            );
            Exposure {
                endpoints: configured_endpoints(cfg),
                enablement: EnablementState::Unknown(error.to_string()),
            }
        }
    }
}

/// Refresh the live catalog from the upstreams and swap it in when it lands.
///
/// A failed refresh leaves the current catalog in place and says so; the
/// snapshot keeps being served.
pub async fn refresh_live_catalog(
    shared: SharedCatalog,
    exposed: Vec<&'static Endpoint>,
    http: reqwest::Client,
) {
    let fallback = Arc::clone(&*shared.read().await);
    match Catalog::build_live(exposed, &http, Some(&fallback)).await {
        Ok(fresh) => {
            let (services, tools) = (fresh.services.len(), fresh.tool_count());
            let diff = fallback.drift_from(&fresh);
            *shared.write().await = Arc::new(fresh);
            tracing::info!(services, tools, "live catalog refreshed and swapped in");
            if !diff.is_empty() {
                tracing::warn!(
                    added = ?diff.added,
                    removed = ?diff.removed,
                    schema_changed = ?diff.schema_changed,
                    "catalog drifted from the committed snapshot; \
                     re-pin it with the `snapshot` subcommand"
                );
            }
        }
        Err(error) => tracing::warn!(
            cause = %error,
            "live catalog refresh failed; continuing to serve the snapshot"
        ),
    }
}

/// The work `serve` takes off its critical path: credentials, enablement,
/// then the live catalog, in that order, publishing readiness as each lands.
///
/// Credentials go first because enablement needs them. Enablement goes before
/// the refresh because pruning shrinks the fan-out from every registered host
/// to the ones actually enabled. Everything here runs after the client's
/// `initialize` has been answered, so none of it competes with the handshake.
pub struct BackgroundStartup {
    /// Catalog and readiness the running server reads from.
    pub state: CatalogState,
    /// Lazily discovered credentials; the first `apply` here is what acquires
    /// them.
    pub auth: Arc<AuthContext>,
    /// Shared client for Service Usage and the refresh fan-out.
    pub http: reqwest::Client,
    /// Resolved configuration.
    pub config: Config,
}

impl BackgroundStartup {
    /// Acquire credentials and resolve enablement, narrowing the live catalog
    /// to the exposed set and publishing readiness at each step. Returns the
    /// endpoints left exposed.
    ///
    /// Never fails: every problem is published as readiness and logged, and
    /// the configured selection stays served.
    pub async fn resolve(&self) -> Vec<&'static Endpoint> {
        let credentials = match self
            .auth
            .apply(&mut reqwest::header::HeaderMap::new())
            .await
        {
            Ok(()) => CredentialState::Ready,
            Err(error) => {
                tracing::warn!(
                    cause = %error,
                    "credentials could not be acquired; every `call` will report this \
                     until a later attempt succeeds"
                );
                CredentialState::Failed(error.to_string())
            }
        };
        self.state.publish_readiness(Readiness {
            credentials: credentials.clone(),
            enablement: EnablementState::Pending,
        });

        // `--only` needs no credentials to decide; anything else does.
        let exposure = match &credentials {
            CredentialState::Failed(reason) if self.config.only.is_empty() => Exposure {
                endpoints: configured_endpoints(&self.config),
                enablement: EnablementState::Unknown(format!("credentials unavailable: {reason}")),
            },
            _ => resolve_enablement(&self.config, &self.auth, &self.http).await,
        };
        if exposure.enablement == EnablementState::Pruned {
            let current = self.state.current().await;
            let narrowed = current.restricted_to(&exposure.endpoints);
            tracing::info!(
                services = narrowed.services.len(),
                tools = narrowed.tool_count(),
                "exposed set narrowed to the enabled APIs"
            );
            *self.state.live.write().await = Arc::new(narrowed);
        }
        self.state.publish_readiness(Readiness {
            credentials,
            enablement: exposure.enablement,
        });
        exposure.endpoints
    }

    /// [`Self::resolve`], then the live catalog refresh for the two-tier
    /// surface (flat mode is pinned at startup and never refreshes).
    pub async fn run(self) {
        let exposed = self.resolve().await;
        match self.config.expose {
            ExposeMode::TwoTier => {
                refresh_live_catalog(self.state.live(), exposed, self.http).await
            }
            ExposeMode::Flat => tracing::debug!(
                "not refreshing the catalog: `--expose flat` pins the tool list at \
                 startup, so a refresh would be discarded"
            ),
        }
    }
}

/// The stdio MCP server aggregating every exposed Google endpoint.
pub struct GoogleMcpServer {
    catalog: CatalogState,
    proxy: Arc<Proxy>,
    expose: ExposeMode,
}

impl GoogleMcpServer {
    /// Assemble the server over a catalog state and a dispatch proxy.
    pub fn new(catalog: CatalogState, proxy: Arc<Proxy>, expose: ExposeMode) -> Self {
        Self {
            catalog,
            proxy,
            expose,
        }
    }

    /// Catalog backing the surface this server exposes.
    ///
    /// Two-tier reads the live catalog; flat is pinned to the startup one, for
    /// the reason [`CatalogState`] documents.
    async fn catalog(&self) -> Arc<Catalog> {
        match self.expose {
            ExposeMode::TwoTier => self.catalog.current().await,
            ExposeMode::Flat => Arc::clone(self.catalog.startup()),
        }
    }

    /// Handle one two-tier meta-tool call.
    async fn call_meta_tool(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        catalog: &Catalog,
    ) -> CallToolResult {
        let args = arguments.unwrap_or_default();
        match name {
            TOOL_LIST_SERVICES => {
                json_result(&list_services_payload(catalog, &self.catalog.readiness()))
            }
            TOOL_SEARCH_TOOLS => {
                let Some(query) = args.get("query").and_then(Value::as_str) else {
                    return missing_argument("query", TOOL_SEARCH_TOOLS);
                };
                let service = args.get("service").and_then(Value::as_str);
                // A caller asking for zero hits is asking a question with no
                // useful answer; floor it at one rather than returning an
                // empty list that reads like "nothing matched".
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map_or(DEFAULT_SEARCH_LIMIT, |n| (n as usize).max(1));
                json_result(&search_payload(catalog, query, service, limit))
            }
            TOOL_DESCRIBE_TOOLS => {
                let Some(names) = args.get("names").and_then(Value::as_array) else {
                    return missing_argument("names", TOOL_DESCRIBE_TOOLS);
                };
                json_result(&describe_payload(catalog, names))
            }
            TOOL_CALL => {
                let Some(target) = args.get("name").and_then(Value::as_str) else {
                    return missing_argument("name", TOOL_CALL);
                };
                if catalog.get(target).is_none() {
                    return CallToolResult::error(vec![ContentBlock::text(not_exposed_message(
                        target, catalog,
                    ))]);
                }
                // A non-object `arguments` used to be dropped silently, so a
                // call written with a list or a JSON string reached the
                // upstream with no arguments at all and failed there, or worse
                // succeeded with defaults. Name the mistake instead.
                let inner = match args.get("arguments") {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(map)) => Some(map.clone()),
                    Some(other) => return invalid_arguments_type(other),
                };
                self.proxy.dispatch(target, inner).await
            }
            other => CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool `{other}`; this server exposes `{TOOL_LIST_SERVICES}`, \
                 `{TOOL_SEARCH_TOOLS}`, `{TOOL_DESCRIBE_TOOLS}`, and `{TOOL_CALL}`"
            ))]),
        }
    }
}

impl ServerHandler for GoogleMcpServer {
    /// Cap negotiation below `2026-07-28`.
    ///
    /// rmcp 3.1.3 will agree to `2026-07-28` when a client offers it, even
    /// though that revision is newer than the SDK's own `LATEST`, and it then
    /// emits a bare `resultType: "complete"` where the revision expects a cache
    /// descriptor carrying `ttlMs` and `cacheScope`. Claude Code offers
    /// `2026-07-28` and rejects the malformed `tools/list` outright, so the
    /// server is reachable but has no usable tools. Nothing here needs anything
    /// newer than `2025-11-25`, so the revision is simply not offered.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2024_11_05,
        ])
    }

    fn get_info(&self) -> InitializeResult {
        let instructions = match self.expose {
            ExposeMode::TwoTier => format!(
                "Aggregates Google Cloud's remote MCP endpoints behind one server. \
                 Tools are namespaced `{{service}}__{{tool}}`. Start with \
                 `{TOOL_LIST_SERVICES}`, narrow with `{TOOL_SEARCH_TOOLS}`, read the \
                 schema with `{TOOL_DESCRIBE_TOOLS}`, then invoke via \
                 `{TOOL_CALL}` with that namespaced name."
            ),
            ExposeMode::Flat => "Aggregates Google Cloud's remote MCP endpoints behind one \
                 server. Every tool is namespaced `{service}__{tool}` and carries its \
                 upstream schema."
                .to_owned(),
        };

        // `InitializeResult` is #[non_exhaustive], so it is built by mutation
        // rather than a struct literal.
        let mut info = InitializeResult::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        info.instructions = Some(instructions);
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let catalog = self.catalog().await;
        let tools = match self.expose {
            ExposeMode::TwoTier => meta_tools(),
            ExposeMode::Flat => flat_tools(&catalog)?,
        };
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let catalog = self.catalog().await;
        let result = match self.expose {
            ExposeMode::TwoTier => {
                self.call_meta_tool(&request.name, request.arguments, &catalog)
                    .await
            }
            ExposeMode::Flat => {
                if catalog.get(&request.name).is_none() {
                    CallToolResult::error(vec![ContentBlock::text(not_exposed_message(
                        &request.name,
                        &catalog,
                    ))])
                } else {
                    self.proxy.dispatch(&request.name, request.arguments).await
                }
            }
        };
        Ok(CallToolResponse::Complete(result))
    }
}

/// The four tools of the default two-tier surface.
///
/// Built once: the descriptions and schemas are constants, and `list_tools` is
/// on the hot path for every client connection.
static META_TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    vec![
        Tool::new(
            TOOL_LIST_SERVICES,
            "List the Google Cloud services exposed by this server, with each one's \
             tool count, Service Usage API name, whether its tools came from a live \
             fetch or the bundled snapshot, and its readiness: credentials and the \
             enabled-API check resolve in the background after startup, so a \
             service is `pending` until they land, `ready` once they have, \
             `unverified` if enablement could not be checked, or `failed` if no \
             credentials could be acquired (the `startup` block carries the reason).",
            schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
        ),
        Tool::new(
            TOOL_SEARCH_TOOLS,
            "Find tools by keyword across every exposed service. Returns namespaced \
             tool names ranked by relevance, each with its `score`, plus \
             `total_matches` when more matched than were returned; pass the name to \
             describe_tools or call.",
            schema(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords; every term must match the tool's name, \
                                        description, or service (`cloud run` names the `run` \
                                        service).",
                    },
                    "service": {
                        "type": "string",
                        "description": "Optional service id to restrict the search to, e.g. `run`.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum hits to return (default 20).",
                    },
                },
                "required": ["query"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            TOOL_DESCRIBE_TOOLS,
            "Return the full input and output JSON schemas for named tools. Call this \
             before `call` so arguments are never guessed.",
            schema(json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Namespaced tool names, e.g. [\"run__list_services\"].",
                    },
                },
                "required": ["names"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            TOOL_CALL,
            "Invoke a namespaced Google Cloud tool and return its result unchanged. \
             Credentials and the quota project are attached automatically.",
            schema(json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Namespaced tool name, e.g. `run__list_services`.",
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments matching that tool's input schema.",
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            })),
        ),
    ]
});

/// The four tools of the default two-tier surface.
fn meta_tools() -> Vec<Tool> {
    META_TOOLS.clone()
}

/// Every exposed tool, renamed to its namespaced form but otherwise verbatim.
///
/// Materializes archived schemas: flat mode hands the client every schema at
/// `initialize`, so this is where the lazy frames are paid for -- by the one
/// mode whose startup is network-bound anyway.
fn flat_tools(catalog: &Catalog) -> Result<Vec<Tool>, McpError> {
    catalog
        .tools()
        .map(|entry| {
            let mut tool = entry.tool.to_rmcp().map_err(|error| {
                tracing::error!(
                    %error,
                    tool = entry.namespaced_name,
                    "flat listing could not materialize a tool definition"
                );
                McpError::internal_error("a tool definition could not be materialized", None)
            })?;
            tool.name = entry.namespaced_name.clone().into();
            Ok(tool)
        })
        .collect()
}

/// Coerce a `json!` object into the shape `Tool::new` wants.
fn schema(value: Value) -> Arc<JsonObject> {
    Arc::new(match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    })
}

/// `list_services` payload.
///
/// Readiness is the same for every service (credentials and the enablement
/// check are process-wide), so it is repeated per entry for a model that
/// reads one service at a time and summarized once in `startup`, with the
/// failure text a caller would otherwise only find in the server's log.
fn list_services_payload(catalog: &Catalog, readiness: &Readiness) -> Value {
    let label = readiness.service_label();
    let services: Vec<Value> = catalog
        .services
        .iter()
        .map(|service| {
            json!({
                "service_id": service.service_id,
                "api_name": crate::registry::find(&service.service_id).map(|e| e.api_name),
                "tool_count": service.tools.len(),
                "source": service.source.to_string(),
                "readiness": label,
            })
        })
        .collect();
    json!({
        "services": services,
        "service_count": catalog.services.len(),
        "tool_count": catalog.tool_count(),
        "startup": {
            "credentials": readiness.credentials.label(),
            "credentials_error": readiness.credentials.detail(),
            "enablement": readiness.enablement.label(),
            "enablement_error": readiness.enablement.detail(),
        },
    })
}

/// `search_tools` payload.
///
/// `score` is relative relevance within this one response; `total_matches`
/// tells the caller how many tools matched before `limit` cut the list, so
/// "narrow the query" is distinguishable from "that was everything".
fn search_payload(catalog: &Catalog, query: &str, service: Option<&str>, limit: usize) -> Value {
    let mut hits = Vec::with_capacity(limit.min(catalog.tool_count()));
    let total = catalog.search_with(query, service, limit, |hit| {
        hits.push(json!({
            "name": hit.tool.namespaced_name,
            "service_id": hit.tool.service_id,
            "score": hit.score,
            "description": hit.tool.tool.description().map(first_line),
        }));
    });
    json!({
        "query": query,
        "match_count": hits.len(),
        "total_matches": total,
        "matches": hits,
    })
}

/// `describe_tools` payload; unknown names are reported rather than skipped.
fn describe_payload(catalog: &Catalog, names: &[Value]) -> Value {
    let mut described = Vec::new();
    let mut unknown = Vec::new();
    for name in names.iter().filter_map(Value::as_str) {
        match catalog.get(name) {
            Some(entry) => match (entry.tool.input_schema(), entry.tool.output_schema()) {
                (Ok(input_schema), Ok(output_schema)) => described.push(json!({
                    "name": entry.namespaced_name,
                    "service_id": entry.service_id,
                    "upstream_name": entry.upstream_name(),
                    "source": catalog.service_of(name).map(|s| s.source.to_string()),
                    "description": entry.tool.description(),
                    "input_schema": input_schema,
                    "output_schema": output_schema,
                })),
                // The archived frames were verified at generation and are
                // pinned to the committed JSON by test, so this branch means
                // the binary and its embedded artifact disagree. Say so
                // instead of panicking or silently omitting the tool.
                (Err(error), _) | (_, Err(error)) => {
                    tracing::error!(%error, tool = name, "describe could not produce schemas");
                    described.push(json!({
                        "name": entry.namespaced_name,
                        "service_id": entry.service_id,
                        "upstream_name": entry.upstream_name(),
                        "source": catalog.service_of(name).map(|s| s.source.to_string()),
                        "description": entry.tool.description(),
                        "schema_error": error.to_string(),
                    }));
                }
            },
            None => unknown.push(name),
        }
    }
    json!({ "tools": described, "unknown": unknown })
}

/// First line of a description, for compact search output.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// Error text for a tool the pruned catalog does not expose.
fn not_exposed_message(name: &str, catalog: &Catalog) -> String {
    let mut services: Vec<&str> = catalog
        .services
        .iter()
        .map(|s| s.service_id.as_str())
        .collect();
    services.sort_unstable();
    format!(
        "`{name}` is not an exposed tool. Exposed services: {}. \
         Use `{TOOL_SEARCH_TOOLS}` to find a tool, or check that its API is enabled \
         on the quota project. The enabled-service list is read once per process, so \
         restart this server after running `gcloud services enable`.",
        services.join(", ")
    )
}

/// Render a payload as pretty JSON text content.
fn json_result(value: &Value) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
            "failed to render result: {error}"
        ))]),
    }
}

/// Error text for a required argument that was absent.
fn missing_argument(argument: &str, tool: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "`{tool}` requires the `{argument}` argument"
    ))])
}

/// Error text for a `call` whose `arguments` is not a JSON object.
///
/// Names the JSON type that arrived rather than quoting the value: the value
/// may be large, and it is the shape that is wrong.
fn invalid_arguments_type(received: &Value) -> CallToolResult {
    let kind = match received {
        Value::Array(_) => "an array",
        Value::String(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        // call_meta_tool routes Null and Object before this point; the arm is
        // kept total rather than unreachable!() so a routing regression
        // degrades to an error message instead of a panic.
        Value::Null | Value::Object(_) => "an unexpected value",
    };
    CallToolResult::error(vec![ContentBlock::text(format!(
        "`{TOOL_CALL}` requires `arguments` to be a JSON object matching the \
         target tool's input schema; received {kind}. Use `{TOOL_DESCRIBE_TOOLS}` \
         to read that schema, and pass the arguments as an object such as \
         {{\"project\": \"my-project\"}}."
    ))])
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin};

    use rmcp::model::Tool as RmcpTool;
    use serde_json::json;

    use super::*;
    use crate::{
        auth::{AuthContext, FetchedToken, TokenSource},
        catalog::{NamespacedTool, ServiceCatalog},
        error::Error,
    };

    fn tool(name: &str, description: &str) -> RmcpTool {
        serde_json::from_value(json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } } },
        }))
        .expect("test tool literal is valid")
    }

    fn catalog() -> Catalog {
        Catalog::new(vec![
            ServiceCatalog {
                service_id: "run".to_owned(),
                source: CatalogSource::Live,
                tools: vec![
                    NamespacedTool::new(
                        "run",
                        tool("list_services", "List Cloud Run services.\nSecond line."),
                    ),
                    NamespacedTool::new("run", tool("get_service", "Get one Cloud Run service.")),
                ],
            },
            ServiceCatalog {
                service_id: "bigquery".to_owned(),
                source: CatalogSource::Snapshot,
                tools: vec![NamespacedTool::new(
                    "bigquery",
                    tool("list_datasets", "List BigQuery datasets."),
                )],
            },
        ])
        .expect("valid test catalog")
    }

    #[test]
    fn protocol_negotiation_never_offers_the_revision_rmcp_serializes_wrongly() {
        // Regression: rmcp 3.1.3 agrees to 2026-07-28 when a client offers it,
        // then emits `resultType: "complete"` where that revision expects a
        // cache descriptor with ttlMs and cacheScope. Claude Code offers
        // 2026-07-28 and rejects the resulting tools/list, leaving the server
        // connected but toolless. Offering the revision at all is the bug.
        let handler = server(catalog(), ExposeMode::TwoTier);
        let offered = ServerHandler::supported_protocol_versions(&handler);
        assert!(
            !offered.contains(&ProtocolVersion::V_2026_07_28),
            "2026-07-28 must not be offered while rmcp serializes its results \
             incorrectly; offered = {offered:?}"
        );
        assert!(
            offered.contains(&ProtocolVersion::V_2025_11_25),
            "the newest revision rmcp serializes correctly must stay on offer; \
             offered = {offered:?}"
        );
    }

    #[test]
    fn two_tier_surface_is_exactly_four_tools() {
        let tools = meta_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                TOOL_LIST_SERVICES,
                TOOL_SEARCH_TOOLS,
                TOOL_DESCRIBE_TOOLS,
                TOOL_CALL
            ]
        );
    }

    #[test]
    fn two_tier_tool_schemas_serialize_as_json_objects() {
        for tool in meta_tools() {
            let encoded = serde_json::to_value(&tool).expect("tool serializes");
            let schema = encoded
                .get("inputSchema")
                .expect("every tool carries an input schema");
            assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
            assert!(
                tool.description.is_some_and(|d| !d.is_empty()),
                "`{}` needs a description for the model to choose it",
                tool.name
            );
        }
    }

    #[test]
    fn flat_mode_renames_tools_but_keeps_their_schemas() {
        let catalog = catalog();
        let tools = flat_tools(&catalog).expect("live schemas always materialize");

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "bigquery__list_datasets",
                "run__get_service",
                "run__list_services"
            ]
        );

        let original = catalog
            .get("run__list_services")
            .expect("present in the catalog");
        let flat = tools
            .iter()
            .find(|t| t.name == "run__list_services")
            .expect("present in flat mode");
        assert_eq!(
            flat.input_schema,
            original.tool.input_schema().expect("live schemas")
        );
        assert_eq!(flat.description.as_deref(), original.tool.description());
    }

    #[test]
    fn list_services_reports_counts_and_source() {
        let payload = list_services_payload(&catalog(), &Readiness::pending());
        assert_eq!(payload["service_count"], json!(2));
        assert_eq!(payload["tool_count"], json!(3));

        let bigquery = &payload["services"][0];
        assert_eq!(bigquery["service_id"], json!("bigquery"));
        assert_eq!(bigquery["source"], json!("snapshot"));
        assert_eq!(bigquery["api_name"], json!("bigquery.googleapis.com"));
        assert_eq!(payload["services"][1]["source"], json!("live"));
    }

    #[test]
    fn list_services_reports_readiness_per_service_and_the_reason_once() {
        let pending = list_services_payload(&catalog(), &Readiness::pending());
        assert_eq!(pending["services"][0]["readiness"], json!("pending"));
        assert_eq!(pending["services"][1]["readiness"], json!("pending"));
        assert_eq!(pending["startup"]["credentials"], json!("pending"));
        assert_eq!(pending["startup"]["enablement"], json!("pending"));
        assert_eq!(pending["startup"]["credentials_error"], Value::Null);

        let failed = list_services_payload(
            &catalog(),
            &Readiness {
                credentials: CredentialState::Failed("no ADC file".to_owned()),
                enablement: EnablementState::Unknown("credentials unavailable".to_owned()),
            },
        );
        assert_eq!(failed["services"][0]["readiness"], json!("failed"));
        assert_eq!(failed["startup"]["credentials"], json!("failed"));
        assert_eq!(failed["startup"]["credentials_error"], json!("no ADC file"));
        assert_eq!(failed["startup"]["enablement"], json!("unknown"));
        assert_eq!(
            failed["startup"]["enablement_error"],
            json!("credentials unavailable")
        );

        let ready = list_services_payload(
            &catalog(),
            &Readiness {
                credentials: CredentialState::Ready,
                enablement: EnablementState::Pruned,
            },
        );
        assert_eq!(ready["services"][1]["readiness"], json!("ready"));
        assert_eq!(ready["startup"]["enablement_error"], Value::Null);
    }

    #[test]
    fn service_readiness_label_follows_credentials_then_enablement() {
        // Credentials decide first: without them nothing can succeed, whatever
        // enablement says. With them, enablement decides between ready,
        // unverified and pending.
        let label = |credentials: CredentialState, enablement: EnablementState| {
            Readiness {
                credentials,
                enablement,
            }
            .service_label()
        };
        let failed = || CredentialState::Failed("x".to_owned());
        let unknown = || EnablementState::Unknown("y".to_owned());

        assert_eq!(label(failed(), EnablementState::Pruned), "failed");
        assert_eq!(label(failed(), EnablementState::Pending), "failed");
        assert_eq!(
            label(CredentialState::Pending, EnablementState::Pruned),
            "pending"
        );
        assert_eq!(
            label(CredentialState::Ready, EnablementState::Pending),
            "pending"
        );
        assert_eq!(label(CredentialState::Ready, unknown()), "unverified");
        assert_eq!(
            label(CredentialState::Ready, EnablementState::Pruned),
            "ready"
        );
        assert_eq!(
            label(CredentialState::Ready, EnablementState::Skipped),
            "ready"
        );
    }

    #[test]
    fn a_freshly_assembled_catalog_is_pending_until_something_publishes() {
        let run = crate::registry::find("run").expect("run is registered");
        let state = assemble_serve_catalog(catalog(), &[run]).expect("valid fixture");
        assert_eq!(state.readiness(), Readiness::pending());

        let watch = state.readiness_watch();
        state.publish_readiness(Readiness {
            credentials: CredentialState::Ready,
            enablement: EnablementState::Skipped,
        });
        assert!(
            watch.has_changed().expect("the sender is alive"),
            "a publish must be observable by a subscriber"
        );
        assert_eq!(state.readiness().service_label(), "ready");
        // A clone shares the channel: readiness is process state, not per-copy.
        assert_eq!(state.clone().readiness().service_label(), "ready");
    }

    fn config(only: &[&str]) -> Config {
        Config {
            quota_project: "test-project".to_owned(),
            only: only.iter().map(|s| (*s).to_owned()).collect(),
            exclude: Vec::new(),
            expose: ExposeMode::TwoTier,
            strict_startup: false,
        }
    }

    #[tokio::test]
    async fn only_skips_service_usage_entirely() {
        // `UnusedTokens` panics if a token is requested, and Service Usage
        // cannot be called without one: finishing at all proves the call was
        // never attempted, which is what the flag's documentation promises.
        let auth = AuthContext::with_source(Arc::new(UnusedTokens), "test-project")
            .expect("a literal project id is a valid header value");
        let exposure = resolve_enablement(&config(&["run"]), &auth, &reqwest::Client::new()).await;

        assert_eq!(exposure.enablement, EnablementState::Skipped);
        let ids: Vec<&str> = exposure.endpoints.iter().map(|e| e.service_id).collect();
        assert_eq!(ids, ["run"]);
    }

    /// A token source whose credentials are broken.
    struct BrokenTokens;

    impl TokenSource for BrokenTokens {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            Box::pin(async { Err(Error::QuotaProjectUnresolved) })
        }
    }

    #[tokio::test]
    async fn background_startup_publishes_failed_credentials_and_keeps_serving() {
        let state = CatalogState::new(catalog());
        let startup = BackgroundStartup {
            state: state.clone(),
            auth: Arc::new(
                AuthContext::with_source(Arc::new(BrokenTokens), "test-project")
                    .expect("a literal project id is a valid header value"),
            ),
            http: reqwest::Client::new(),
            // `--only` keeps enablement off the network too, so this resolves
            // without any upstream at all.
            config: config(&["run", "bigquery"]),
        };

        let exposed = startup.resolve().await;

        let readiness = state.readiness();
        assert!(
            matches!(&readiness.credentials, CredentialState::Failed(reason) if reason.contains("quota project")),
            "the failure text the caller will see must be published: {readiness:?}"
        );
        assert_eq!(readiness.enablement, EnablementState::Skipped);
        assert_eq!(readiness.service_label(), "failed");
        assert_eq!(exposed.len(), 2);
        // Nothing was pruned: the configured catalog is still what is served.
        assert_eq!(state.current().await.tool_count(), 3);
    }

    #[test]
    fn search_returns_one_line_descriptions_and_honors_limit() {
        let catalog = catalog();
        let payload = search_payload(&catalog, "list", None, 20);
        assert_eq!(payload["match_count"], json!(2));
        assert_eq!(payload["total_matches"], json!(2));
        assert_eq!(
            payload["matches"][0]["description"],
            json!("List BigQuery datasets.")
        );

        let limited = search_payload(&catalog, "list", None, 1);
        assert_eq!(limited["match_count"], json!(1));
        assert_eq!(
            limited["total_matches"],
            json!(2),
            "the caller must be able to tell a cut list from a complete one"
        );

        let filtered = search_payload(&catalog, "list", Some("run"), 20);
        assert_eq!(filtered["match_count"], json!(1));
        assert_eq!(filtered["matches"][0]["name"], json!("run__list_services"));
        // Multi-line descriptions collapse to their first line.
        assert_eq!(
            filtered["matches"][0]["description"],
            json!("List Cloud Run services.")
        );
    }

    #[test]
    fn search_matches_carry_scores_in_descending_order() {
        let payload = search_payload(&catalog(), "list", None, 20);
        let scores: Vec<u64> = payload["matches"]
            .as_array()
            .expect("matches is an array")
            .iter()
            .map(|hit| hit["score"].as_u64().expect("every match carries a score"))
            .collect();
        assert!(!scores.is_empty());
        assert!(
            scores.windows(2).all(|pair| pair[0] >= pair[1]),
            "matches must be ordered best first, got {scores:?}"
        );
        assert!(
            scores.iter().all(|&score| score > 0),
            "a non-empty query only returns scored hits, got {scores:?}"
        );
    }

    /// An archived tool whose frames will not inflate is described with a
    /// `schema_error` instead of being silently dropped or panicking; the
    /// condition means the binary and its embedded artifact disagree.
    #[test]
    fn describe_reports_a_schema_error_instead_of_dropping_the_tool() {
        let catalog = Catalog::new(vec![crate::catalog::ServiceCatalog {
            service_id: "run".to_owned(),
            source: CatalogSource::Snapshot,
            tools: vec![crate::catalog::NamespacedTool {
                namespaced_name: "run__broken".to_owned(),
                service_id: "run".to_owned(),
                tool: crate::catalog::ToolSpec::broken_for_tests("broken"),
            }],
        }])
        .expect("valid");

        let payload = describe_payload(&catalog, &[json!("run__broken")]);
        assert_eq!(payload["unknown"], json!([]));
        assert_eq!(payload["tools"][0]["name"], json!("run__broken"));
        assert!(
            payload["tools"][0]["schema_error"]
                .as_str()
                .is_some_and(|text| text.contains("broken")),
            "got {payload}"
        );
        assert!(payload["tools"][0].get("input_schema").is_none());
    }

    #[test]
    fn describe_separates_known_from_unknown_names() {
        let payload = describe_payload(
            &catalog(),
            &[json!("run__list_services"), json!("run__nope"), json!(42)],
        );
        assert_eq!(payload["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(payload["tools"][0]["upstream_name"], json!("list_services"));
        assert!(payload["tools"][0]["input_schema"].is_object());
        assert_eq!(payload["unknown"], json!(["run__nope"]));
    }

    #[test]
    fn not_exposed_message_lists_available_services() {
        let message = not_exposed_message("compute__list", &catalog());
        assert!(message.contains("compute__list"));
        assert!(message.contains("bigquery, run"));
    }

    #[test]
    fn first_line_trims() {
        assert_eq!(first_line("  one  \ntwo"), "one");
        assert_eq!(first_line("only"), "only");
        assert_eq!(first_line(""), "");
    }

    /// A token source that must never be asked for a token.
    ///
    /// The argument checks below reject their input before any dispatch, so a
    /// fetch here means the code under test let a bad call through.
    struct UnusedTokens;

    impl TokenSource for UnusedTokens {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            Box::pin(async { panic!("a rejected call must never reach dispatch") })
        }
    }

    /// A server over `catalog` whose proxy routes nowhere.
    fn server(catalog: Catalog, expose: ExposeMode) -> GoogleMcpServer {
        let auth = Arc::new(
            AuthContext::with_source(Arc::new(UnusedTokens), "test-project")
                .expect("a literal project id is a valid header value"),
        );
        GoogleMcpServer::new(
            CatalogState::new(catalog),
            Arc::new(Proxy::new(auth, reqwest::Client::new(), Vec::new())),
            expose,
        )
    }

    /// `json!` object literal as the argument map a tool call carries.
    fn args(value: Value) -> JsonObject {
        value
            .as_object()
            .cloned()
            .expect("test argument literals are objects")
    }

    /// Concatenated text of a result, for assertions.
    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_search_limit_of_zero_is_floored_to_one_hit() {
        let server = server(catalog(), ExposeMode::TwoTier);
        let catalog = catalog();

        let result = server
            .call_meta_tool(
                TOOL_SEARCH_TOOLS,
                Some(args(json!({ "query": "list", "limit": 0 }))),
                &catalog,
            )
            .await;

        let payload: Value =
            serde_json::from_str(&text_of(&result)).expect("search returns a JSON payload");
        assert_eq!(
            payload["match_count"],
            json!(1),
            "a zero limit must return one hit rather than read as `nothing \
             matched`: {payload}"
        );
    }

    #[tokio::test]
    async fn call_rejects_a_non_object_arguments_value() {
        let server = server(catalog(), ExposeMode::TwoTier);
        let catalog = catalog();

        for (value, expected) in [
            (json!([{ "project": "p" }]), "an array"),
            (json!("project=p"), "a string"),
            (json!(42), "a number"),
            (json!(true), "a boolean"),
        ] {
            let result = server
                .call_meta_tool(
                    TOOL_CALL,
                    Some(args(
                        json!({ "name": "run__list_services", "arguments": value }),
                    )),
                    &catalog,
                )
                .await;

            assert_eq!(
                result.is_error,
                Some(true),
                "`arguments` of the wrong type must be reported, not dropped"
            );
            let text = text_of(&result);
            assert!(
                text.contains(expected),
                "the error must name the type that arrived ({expected}): {text}"
            );
            assert!(
                text.contains(TOOL_DESCRIBE_TOOLS),
                "the error must point at the schema to read: {text}"
            );
        }
    }

    #[tokio::test]
    async fn an_absent_or_null_arguments_value_dispatches_with_no_arguments() {
        // `call` with no arguments is legitimate for a tool that takes none,
        // so neither absence nor an explicit null may be treated as an error.
        // Dispatch has no route for `run`, so the failure below is the routing
        // message, which is proof that the argument check let the call through.
        let server = server(catalog(), ExposeMode::TwoTier);
        let catalog = catalog();

        for arguments in [
            json!({ "name": "run__list_services" }),
            json!({ "name": "run__list_services", "arguments": null }),
        ] {
            let result = server
                .call_meta_tool(TOOL_CALL, Some(args(arguments)), &catalog)
                .await;
            let text = text_of(&result);
            assert!(
                text.contains("unknown service"),
                "the call must reach dispatch and fail on routing, not on its \
                 arguments: {text}"
            );
        }
    }

    #[tokio::test]
    async fn flat_mode_serves_the_startup_catalog_even_after_a_refresh() {
        // Flat mode published these tool names to the client at initialize and
        // this server has no `listChanged` to correct them with, so a refresh
        // must not move them underneath the client.
        let state = CatalogState::new(catalog());
        let auth = Arc::new(
            AuthContext::with_source(Arc::new(UnusedTokens), "test-project")
                .expect("a literal project id is a valid header value"),
        );
        let flat = GoogleMcpServer::new(
            state.clone(),
            Arc::new(Proxy::new(auth, reqwest::Client::new(), Vec::new())),
            ExposeMode::Flat,
        );

        let startup = flat.catalog().await;
        let before: Vec<String> = flat_tools(&startup)
            .expect("live schemas always materialize")
            .iter()
            .map(|t| t.name.to_string())
            .collect();

        // A refresh lands, dropping `run` entirely.
        let refreshed = Catalog::new(vec![ServiceCatalog {
            service_id: "bigquery".to_owned(),
            source: CatalogSource::Live,
            tools: vec![NamespacedTool::new(
                "bigquery",
                tool("list_datasets", "List BigQuery datasets."),
            )],
        }])
        .expect("valid");
        *state.live().write().await = Arc::new(refreshed);

        let served_now = flat.catalog().await;
        let after: Vec<String> = flat_tools(&served_now)
            .expect("live schemas always materialize")
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            before, after,
            "the flat tool list is fixed at startup; the refresh must not \
             change what the client was told"
        );
        assert!(after.iter().any(|name| name.starts_with("run__")));
    }

    #[tokio::test]
    async fn two_tier_mode_sees_the_refreshed_catalog() {
        // The counterpart to the test above: the meta-tools describe whatever
        // is current, because their own four names never change.
        let state = CatalogState::new(catalog());
        let auth = Arc::new(
            AuthContext::with_source(Arc::new(UnusedTokens), "test-project")
                .expect("a literal project id is a valid header value"),
        );
        let two_tier = GoogleMcpServer::new(
            state.clone(),
            Arc::new(Proxy::new(auth, reqwest::Client::new(), Vec::new())),
            ExposeMode::TwoTier,
        );

        let refreshed = Catalog::new(vec![ServiceCatalog {
            service_id: "bigquery".to_owned(),
            source: CatalogSource::Live,
            tools: vec![NamespacedTool::new(
                "bigquery",
                tool("list_datasets", "List BigQuery datasets."),
            )],
        }])
        .expect("valid");
        *state.live().write().await = Arc::new(refreshed);

        let current = two_tier.catalog().await;
        let payload = list_services_payload(&current, &Readiness::pending());
        assert_eq!(
            payload["service_count"],
            json!(1),
            "two-tier must answer from the live catalog: {payload}"
        );
    }

    #[test]
    fn assembling_the_serve_catalog_relabels_it_as_snapshot_sourced() {
        // The file records `live`, because that is what its capture run saw.
        let snapshot = catalog().to_snapshot("2026-08-19T00:00:00Z".to_owned());
        assert!(
            snapshot
                .services
                .iter()
                .any(|s| s.source == CatalogSource::Live),
            "sanity: the fixture must carry the provenance the relabel has to \
             overwrite, or this test proves nothing"
        );

        let run = crate::registry::find("run").expect("run is registered");
        let bigquery = crate::registry::find("bigquery").expect("bigquery is registered");
        let state = assemble_serve_catalog(
            snapshot.into_catalog().expect("fixture is valid"),
            &[run, bigquery],
        )
        .expect("the fixture satisfies the namespacing invariants");

        for service in &state.startup().services {
            assert_eq!(
                service.source,
                CatalogSource::Snapshot,
                "service `{}` has not been fetched by this process, so the \
                 serve assembly must report snapshot provenance",
                service.service_id
            );
        }
    }

    #[test]
    fn missing_argument_is_an_error_result_naming_the_argument() {
        let result = missing_argument("query", TOOL_SEARCH_TOOLS);
        assert_eq!(result.is_error, Some(true));
        let rendered = serde_json::to_string(&result.content).expect("content serializes");
        assert!(rendered.contains("query"));
        assert!(rendered.contains(TOOL_SEARCH_TOOLS));
    }
}
