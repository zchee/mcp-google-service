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
use tokio::sync::RwLock;

use crate::{
    catalog::{Catalog, CatalogError, CatalogSource, Snapshot},
    config::ExposeMode,
    proxy::Proxy,
    registry::Endpoint,
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
}

impl CatalogState {
    /// Freeze `catalog` as the startup surface and start serving it live.
    pub fn new(catalog: Catalog) -> Self {
        let startup = Arc::new(catalog);
        Self {
            live: Arc::new(RwLock::new(Arc::clone(&startup))),
            startup,
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
    snapshot: Snapshot,
    exposed: &[&Endpoint],
) -> Result<CatalogState, CatalogError> {
    Ok(CatalogState::new(
        snapshot
            .into_catalog()?
            .restricted_to(exposed)
            .marked_as(CatalogSource::Snapshot),
    ))
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
            TOOL_LIST_SERVICES => json_result(&list_services_payload(catalog)),
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
            ExposeMode::Flat => flat_tools(&catalog),
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
             tool count, Service Usage API name, and whether its tools came from a \
             live fetch or the bundled snapshot.",
            schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
        ),
        Tool::new(
            TOOL_SEARCH_TOOLS,
            "Find tools by keyword across every exposed service. Returns namespaced \
             tool names ranked by relevance; pass the name to describe_tools or call.",
            schema(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords; every term must match a tool's name or description.",
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
fn flat_tools(catalog: &Catalog) -> Vec<Tool> {
    catalog
        .tools()
        .map(|entry| {
            let mut tool = entry.tool.clone();
            tool.name = entry.namespaced_name.clone().into();
            tool
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
fn list_services_payload(catalog: &Catalog) -> Value {
    let services: Vec<Value> = catalog
        .services
        .iter()
        .map(|service| {
            json!({
                "service_id": service.service_id,
                "api_name": crate::registry::find(&service.service_id).map(|e| e.api_name),
                "tool_count": service.tools.len(),
                "source": service.source.to_string(),
            })
        })
        .collect();
    json!({
        "services": services,
        "service_count": catalog.services.len(),
        "tool_count": catalog.tool_count(),
    })
}

/// `search_tools` payload.
fn search_payload(catalog: &Catalog, query: &str, service: Option<&str>, limit: usize) -> Value {
    let hits: Vec<Value> = catalog
        .search(query, service)
        .into_iter()
        .take(limit)
        .map(|entry| {
            json!({
                "name": entry.namespaced_name,
                "service_id": entry.service_id,
                "description": entry.tool.description.as_deref().map(first_line),
            })
        })
        .collect();
    json!({ "query": query, "match_count": hits.len(), "matches": hits })
}

/// `describe_tools` payload; unknown names are reported rather than skipped.
fn describe_payload(catalog: &Catalog, names: &[Value]) -> Value {
    let mut described = Vec::new();
    let mut unknown = Vec::new();
    for name in names.iter().filter_map(Value::as_str) {
        match catalog.get(name) {
            Some(entry) => described.push(json!({
                "name": entry.namespaced_name,
                "service_id": entry.service_id,
                "upstream_name": entry.upstream_name(),
                "source": catalog.service_of(name).map(|s| s.source.to_string()),
                "description": entry.tool.description,
                "input_schema": entry.tool.input_schema,
                "output_schema": entry.tool.output_schema,
            })),
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
         on the quota project. The enabled-service list is read once at startup, so \
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
        let tools = flat_tools(&catalog);

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
        assert_eq!(flat.input_schema, original.tool.input_schema);
        assert_eq!(flat.description, original.tool.description);
    }

    #[test]
    fn list_services_reports_counts_and_source() {
        let payload = list_services_payload(&catalog());
        assert_eq!(payload["service_count"], json!(2));
        assert_eq!(payload["tool_count"], json!(3));

        let bigquery = &payload["services"][0];
        assert_eq!(bigquery["service_id"], json!("bigquery"));
        assert_eq!(bigquery["source"], json!("snapshot"));
        assert_eq!(bigquery["api_name"], json!("bigquery.googleapis.com"));
        assert_eq!(payload["services"][1]["source"], json!("live"));
    }

    #[test]
    fn search_returns_one_line_descriptions_and_honors_limit() {
        let catalog = catalog();
        let payload = search_payload(&catalog, "list", None, 20);
        assert_eq!(payload["match_count"], json!(2));
        assert_eq!(
            payload["matches"][0]["description"],
            json!("List BigQuery datasets.")
        );

        let limited = search_payload(&catalog, "list", None, 1);
        assert_eq!(limited["match_count"], json!(1));

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
        let payload = list_services_payload(&current);
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
        let state = assemble_serve_catalog(snapshot, &[run, bigquery])
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
