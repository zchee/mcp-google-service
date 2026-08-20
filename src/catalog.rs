//! Tool catalog fan-out, namespacing, and snapshot handling.
//!
//! Discovery against Google's remote MCP endpoints is unauthenticated: every
//! probed host answers `initialize` and `tools/list` with no credentials. That
//! property is what lets the catalog be fetched at startup, committed as a
//! snapshot, and embedded in the binary as an offline fallback.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::Duration,
};

use rmcp::{
    ServiceExt,
    model::Tool,
    service::{ClientInitializeError, ServiceError},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, task::JoinSet, time::timeout};

use crate::registry::{self, Endpoint};

/// Separator between the service prefix and the upstream tool name.
///
/// Doubled because single underscores occur inside both halves; service ids
/// never contain `__`, so the first occurrence is always the split point.
pub const NAMESPACE_SEPARATOR: &str = "__";

/// Maximum accepted length of a namespaced tool name.
///
/// 64 is the limit commonly enforced by MCP clients on tool names.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Number of upstreams contacted concurrently during a catalog fan-out.
pub const FETCH_CONCURRENCY: usize = 16;

/// Per-host budget covering `initialize` plus every `tools/list` page.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Repository-relative location of the committed snapshot.
pub const SNAPSHOT_PATH: &str = "data/catalog-snapshot.json";

/// Snapshot compiled into the binary so a network-less start still serves tools.
const EMBEDDED_SNAPSHOT: &str = include_str!("../data/catalog-snapshot.json");

/// Score added when a query token occurs in a tool's name.
const SCORE_NAME_MATCH: u32 = 8;

/// Score added when a query token occurs in a tool's description.
const SCORE_DESCRIPTION_MATCH: u32 = 2;

/// Bonus applied when the whole query equals the bare (un-prefixed) tool name.
const SCORE_EXACT_NAME: u32 = 32;

/// Failures that make a catalog unusable.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// Two services contributed the same namespaced tool name.
    #[error(
        "namespaced tool name `{name}` is claimed by both `{first}` and `{second}`; \
         the `{{service}}__{{tool}}` scheme must be globally unique"
    )]
    DuplicateToolName {
        /// The colliding namespaced name.
        name: String,
        /// Service that registered the name first.
        first: String,
        /// Service that collided with it.
        second: String,
    },

    /// A namespaced name exceeded the client-side length limit.
    #[error(
        "namespaced tool name `{name}` is {len} chars, over the {MAX_TOOL_NAME_LEN}-char limit"
    )]
    ToolNameTooLong {
        /// The offending namespaced name.
        name: String,
        /// Its length in characters.
        len: usize,
    },

    /// The snapshot baked into the binary is not valid JSON for this model.
    #[error("embedded snapshot at `{SNAPSHOT_PATH}` is not a valid catalog snapshot")]
    EmbeddedSnapshot(#[source] serde_json::Error),

    /// An explicitly named snapshot file could not be read.
    #[error("catalog snapshot `{path}` could not be read")]
    SnapshotUnreadable {
        /// Path as the operator gave it.
        path: String,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// An explicitly named snapshot file is not a valid snapshot.
    #[error("catalog snapshot `{path}` is not a valid catalog snapshot")]
    SnapshotInvalid {
        /// Path as the operator gave it.
        path: String,
        /// Parse failure cause.
        #[source]
        source: serde_json::Error,
    },

    /// The snapshot could not be serialized.
    #[error("failed to serialize catalog snapshot")]
    Serialize(#[source] serde_json::Error),
}

/// Why a service's tools carry the contents they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogSource {
    /// Fetched from the upstream during this process's lifetime.
    Live,
    /// Restored from the committed snapshot because the live fetch failed.
    Snapshot,
}

impl std::fmt::Display for CatalogSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Live => f.write_str("live"),
            Self::Snapshot => f.write_str("snapshot"),
        }
    }
}

/// One upstream tool, addressed by its globally unique namespaced name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamespacedTool {
    /// `{service_id}__{tool.name}`; unique across the whole catalog.
    pub namespaced_name: String,
    /// Service that serves this tool.
    pub service_id: String,
    /// The upstream tool definition, including its input/output schemas.
    pub tool: Tool,
}

impl NamespacedTool {
    /// Build the namespaced view of an upstream tool.
    pub fn new(service_id: &str, tool: Tool) -> Self {
        Self {
            namespaced_name: format!("{service_id}{NAMESPACE_SEPARATOR}{}", tool.name),
            service_id: service_id.to_owned(),
            tool,
        }
    }

    /// The upstream tool name, without the service prefix.
    pub fn upstream_name(&self) -> &str {
        &self.tool.name
    }
}

/// Every tool a single service exposes, plus where those tools came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceCatalog {
    /// Registry service id (e.g. `run`).
    pub service_id: String,
    /// Whether these tools are live or snapshot-restored.
    pub source: CatalogSource,
    /// The service's namespaced tools, sorted by namespaced name.
    pub tools: Vec<NamespacedTool>,
}

/// The merged, namespace-validated tool catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// Per-service catalogs, sorted by service id.
    pub services: Vec<ServiceCatalog>,
}

/// What changed between two catalogs, excluding description text.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CatalogDrift {
    /// Namespaced names present only in the newer catalog.
    pub added: Vec<String>,
    /// Namespaced names present only in the older catalog.
    pub removed: Vec<String>,
    /// Namespaced names whose input or output schema changed.
    pub schema_changed: Vec<String>,
}

impl CatalogDrift {
    /// Whether the two catalogs agree on names and schemas.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.schema_changed.is_empty()
    }
}

/// On-disk snapshot model written by the `snapshot` subcommand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// RFC 3339 timestamp of when the snapshot was taken.
    pub generated_at: String,
    /// Per-service catalogs, sorted by service id.
    pub services: Vec<ServiceCatalog>,
}

impl Snapshot {
    /// Validate the snapshot's namespacing and turn it into a [`Catalog`].
    pub fn into_catalog(self) -> Result<Catalog, CatalogError> {
        Catalog::new(self.services)
    }
}

/// Reasons a single upstream failed to answer discovery.
#[derive(Debug, thiserror::Error)]
enum FetchError {
    /// `initialize` never completed.
    #[error("MCP initialize failed")]
    Initialize(#[source] Box<ClientInitializeError>),

    /// `tools/list` failed after a successful handshake.
    #[error("tools/list failed")]
    ListTools(#[source] Box<ServiceError>),

    /// The host exceeded [`FETCH_TIMEOUT`].
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The bounded-concurrency permit could not be acquired.
    #[error("fan-out semaphore closed before this host was contacted")]
    Cancelled,
}

impl Catalog {
    /// Sort and validate per-service catalogs into a usable catalog.
    ///
    /// Sorting is what makes both the snapshot bytes and search results stable
    /// across runs, so a regenerated snapshot produces a meaningful `git diff`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::DuplicateToolName`] if two services produce the
    /// same namespaced name, or [`CatalogError::ToolNameTooLong`] if any name
    /// exceeds [`MAX_TOOL_NAME_LEN`].
    pub fn new(mut services: Vec<ServiceCatalog>) -> Result<Self, CatalogError> {
        services.sort_by(|a, b| a.service_id.cmp(&b.service_id));
        for service in &mut services {
            service
                .tools
                .sort_by(|a, b| a.namespaced_name.cmp(&b.namespaced_name));
        }

        let catalog = Self { services };
        catalog.validate()?;
        Ok(catalog)
    }

    /// Enforce the two namespacing invariants over the whole catalog.
    fn validate(&self) -> Result<(), CatalogError> {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for service in &self.services {
            for tool in &service.tools {
                let name = tool.namespaced_name.as_str();
                let len = name.chars().count();
                if len > MAX_TOOL_NAME_LEN {
                    return Err(CatalogError::ToolNameTooLong {
                        name: name.to_owned(),
                        len,
                    });
                }
                if let Some(first) = seen.insert(name, service.service_id.as_str()) {
                    return Err(CatalogError::DuplicateToolName {
                        name: name.to_owned(),
                        first: first.to_owned(),
                        second: service.service_id.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Fetch the catalog from the given endpoints without credentials.
    ///
    /// Hosts are contacted at [`FETCH_CONCURRENCY`] at a time with a
    /// [`FETCH_TIMEOUT`] budget each. A host that fails degrades to its
    /// `fallback` entry with a `WARN` naming the host and the cause; it is
    /// never fatal. With no fallback entry the service is simply absent.
    ///
    /// # Errors
    ///
    /// Only namespacing-invariant violations, per [`Catalog::new`].
    pub async fn build_live(
        endpoints: impl IntoIterator<Item = &'static Endpoint>,
        http: &reqwest::Client,
        fallback: Option<&Catalog>,
    ) -> Result<Self, CatalogError> {
        let permits = Arc::new(Semaphore::new(FETCH_CONCURRENCY));
        let mut tasks = JoinSet::new();

        for endpoint in endpoints {
            let permits = Arc::clone(&permits);
            // Cloning a `reqwest::Client` clones an `Arc`, so every task shares
            // one connection pool instead of building its own TLS stack.
            let http = http.clone();
            tasks.spawn(async move {
                let outcome = match permits.acquire_owned().await {
                    Ok(_permit) => match timeout(FETCH_TIMEOUT, fetch_tools(endpoint, &http)).await
                    {
                        Ok(result) => result,
                        Err(_elapsed) => Err(FetchError::Timeout(FETCH_TIMEOUT)),
                    },
                    Err(_closed) => Err(FetchError::Cancelled),
                };
                (endpoint, outcome)
            });
        }

        let mut services = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let (endpoint, outcome) = match joined {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "catalog fan-out task failed to join");
                    continue;
                }
            };

            match outcome {
                Ok(tools) => {
                    tracing::debug!(
                        service = endpoint.service_id,
                        host = endpoint.host,
                        tools = tools.len(),
                        "fetched upstream tool list"
                    );
                    services.push(ServiceCatalog {
                        service_id: endpoint.service_id.to_owned(),
                        source: CatalogSource::Live,
                        tools: tools
                            .into_iter()
                            .map(|tool| NamespacedTool::new(endpoint.service_id, tool))
                            .collect(),
                    });
                }
                Err(error) => match fallback.and_then(|c| c.service(endpoint.service_id)) {
                    Some(stale) => {
                        tracing::warn!(
                            host = endpoint.host,
                            cause = %error,
                            tools = stale.tools.len(),
                            "live discovery failed; serving this service from the snapshot"
                        );
                        services.push(ServiceCatalog {
                            service_id: stale.service_id.clone(),
                            source: CatalogSource::Snapshot,
                            tools: stale.tools.clone(),
                        });
                    }
                    None => {
                        tracing::warn!(
                            host = endpoint.host,
                            cause = %error,
                            "live discovery failed and no snapshot entry exists; service omitted"
                        );
                    }
                },
            }
        }

        Self::new(services)
    }

    /// Look up a service's catalog by id.
    pub fn service(&self, service_id: &str) -> Option<&ServiceCatalog> {
        self.services.iter().find(|s| s.service_id == service_id)
    }

    /// Total number of tools across every service.
    pub fn tool_count(&self) -> usize {
        self.services.iter().map(|s| s.tools.len()).sum()
    }

    /// Every namespaced tool, in catalog order.
    pub fn tools(&self) -> impl Iterator<Item = &NamespacedTool> {
        self.services.iter().flat_map(|s| s.tools.iter())
    }

    /// Resolve a namespaced name to its tool.
    pub fn get(&self, namespaced_name: &str) -> Option<&NamespacedTool> {
        let service_id = split_namespaced(namespaced_name)?.0;
        self.service(service_id)?
            .tools
            .iter()
            .find(|t| t.namespaced_name == namespaced_name)
    }

    /// Resolve a namespaced name to the service that serves it.
    ///
    /// Returns `None` when the name is unprefixed or names an absent service.
    pub fn service_of(&self, namespaced_name: &str) -> Option<&ServiceCatalog> {
        let service_id = split_namespaced(namespaced_name)?.0;
        self.service(service_id)
            .filter(|s| s.tools.iter().any(|t| t.namespaced_name == namespaced_name))
    }

    /// Rank tools by keyword overlap with `query` over names and descriptions.
    ///
    /// Matching is case-insensitive and conjunctive: every whitespace-separated
    /// token in `query` must occur in the tool's name or description, so extra
    /// terms narrow rather than widen the result. Name hits outweigh
    /// description hits, and an exact bare-name match sorts to the top. Ties
    /// break on namespaced name, making the order fully deterministic.
    ///
    /// An empty query returns every tool permitted by `service_filter`.
    pub fn search(&self, query: &str, service_filter: Option<&str>) -> Vec<&NamespacedTool> {
        let query = query.trim().to_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();

        let mut scored: Vec<(u32, &NamespacedTool)> = self
            .services
            .iter()
            .filter(|s| service_filter.is_none_or(|f| s.service_id == f))
            .flat_map(|s| s.tools.iter())
            .filter_map(|tool| score_tool(tool, &query, &tokens).map(|score| (score, tool)))
            .collect();

        scored.sort_by(|a, b| {
            Reverse(a.0)
                .cmp(&Reverse(b.0))
                .then_with(|| a.1.namespaced_name.cmp(&b.1.namespaced_name))
        });
        scored.into_iter().map(|(_, tool)| tool).collect()
    }

    /// A copy holding only the services in `endpoints`.
    ///
    /// Used to narrow a full snapshot down to the pruned set actually exposed.
    pub fn restricted_to(&self, endpoints: &[&Endpoint]) -> Self {
        Self {
            services: self
                .services
                .iter()
                .filter(|service| endpoints.iter().any(|e| e.service_id == service.service_id))
                .cloned()
                .collect(),
        }
    }

    /// A copy with every service relabelled as coming from `source`.
    ///
    /// The snapshot file records how each entry was obtained *when the snapshot
    /// was generated*, which is almost always `Live`. A process serving those
    /// bytes must not repeat that claim: until its own refresh lands, the tools
    /// it is serving really did come from the snapshot, and saying so is what
    /// keeps a stale catalog from looking fresh.
    pub fn marked_as(&self, source: CatalogSource) -> Self {
        Self {
            services: self
                .services
                .iter()
                .map(|service| ServiceCatalog {
                    source,
                    ..service.clone()
                })
                .collect(),
        }
    }

    /// Compare against a newer catalog, ignoring description text.
    ///
    /// Descriptions are excluded deliberately: at least one upstream
    /// (`cloudcli`) serves different description text from different backend
    /// replicas, so diffing on it reports drift on almost every refresh. Tool
    /// names and schemas are stable and are what actually affect callers.
    pub fn drift_from(&self, newer: &Self) -> CatalogDrift {
        let old: BTreeMap<&str, &NamespacedTool> = self.by_name();
        let new: BTreeMap<&str, &NamespacedTool> = newer.by_name();

        CatalogDrift {
            added: new
                .keys()
                .filter(|name| !old.contains_key(*name))
                .map(|name| (*name).to_owned())
                .collect(),
            removed: old
                .keys()
                .filter(|name| !new.contains_key(*name))
                .map(|name| (*name).to_owned())
                .collect(),
            schema_changed: old
                .iter()
                .filter_map(|(name, before)| {
                    let after = new.get(name)?;
                    let changed = before.tool.input_schema != after.tool.input_schema
                        || before.tool.output_schema != after.tool.output_schema;
                    changed.then(|| (*name).to_owned())
                })
                .collect(),
        }
    }

    /// Index of every tool by namespaced name.
    fn by_name(&self) -> BTreeMap<&str, &NamespacedTool> {
        self.tools()
            .map(|tool| (tool.namespaced_name.as_str(), tool))
            .collect()
    }

    /// Pair this catalog with a timestamp for serialization.
    pub fn to_snapshot(&self, generated_at: String) -> Snapshot {
        Snapshot {
            generated_at,
            services: self.services.clone(),
        }
    }

    /// Render this catalog as newline-terminated snapshot JSON.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Serialize`] if the catalog cannot be encoded.
    pub fn to_snapshot_json(&self, generated_at: String) -> Result<String, CatalogError> {
        let mut json = serde_json::to_string_pretty(&self.to_snapshot(generated_at))
            .map_err(CatalogError::Serialize)?;
        json.push('\n');
        Ok(json)
    }
}

/// Split `{service}__{tool}` into its halves.
///
/// Service ids never contain `__`, so the first separator is the boundary even
/// when the upstream tool name contains one.
pub fn split_namespaced(namespaced_name: &str) -> Option<(&str, &str)> {
    let (service_id, tool_name) = namespaced_name.split_once(NAMESPACE_SEPARATOR)?;
    if service_id.is_empty() || tool_name.is_empty() {
        return None;
    }
    Some((service_id, tool_name))
}

/// Score one tool against the lowercased query, or `None` if a token misses.
fn score_tool(tool: &NamespacedTool, query: &str, tokens: &[&str]) -> Option<u32> {
    if tokens.is_empty() {
        return Some(0);
    }

    let name = tool.tool.name.to_lowercase();
    let description = tool
        .tool
        .description
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_default();

    let mut total = 0;
    for token in tokens {
        let mut hit = 0;
        if name.contains(token) {
            hit += SCORE_NAME_MATCH;
        }
        if description.contains(token) {
            hit += SCORE_DESCRIPTION_MATCH;
        }
        if hit == 0 {
            return None;
        }
        total += hit;
    }

    if name == query {
        total += SCORE_EXACT_NAME;
    }
    Some(total)
}

/// Run `initialize` + `tools/list` against one endpoint with no credentials.
async fn fetch_tools(
    endpoint: &'static Endpoint,
    http: &reqwest::Client,
) -> Result<Vec<Tool>, FetchError> {
    // `from_uri` would build a fresh client per host, paying TLS setup 47 times
    // per refresh; `with_client` reuses the process-wide pool instead.
    let transport = StreamableHttpClientTransport::with_client(
        http.clone(),
        StreamableHttpClientTransportConfig::with_uri(endpoint.mcp_url()),
    );
    let client = ().serve(transport).await.map_err(|e| FetchError::Initialize(Box::new(e)))?;

    let listed = client.list_all_tools().await;
    // Tear the session down regardless of the listing outcome; a failed
    // shutdown of a discovery session is not worth surfacing.
    let _ = client.cancel().await;

    listed.map_err(|e| FetchError::ListTools(Box::new(e)))
}

/// Parse the snapshot compiled into the binary.
///
/// # Errors
///
/// Returns [`CatalogError::EmbeddedSnapshot`] when the embedded copy fails to
/// parse, which is a build-time defect rather than a runtime condition.
pub fn embedded_snapshot() -> Result<Snapshot, CatalogError> {
    serde_json::from_str(EMBEDDED_SNAPSHOT).map_err(CatalogError::EmbeddedSnapshot)
}

/// Load a snapshot from an explicitly named file, with no fallback.
///
/// Failure is fatal on purpose: an operator who names a snapshot has said
/// which tool metadata to serve, and silently substituting different bytes
/// would answer a different question than the one they asked.
///
/// # Errors
///
/// [`CatalogError::SnapshotUnreadable`] if the file cannot be read, or
/// [`CatalogError::SnapshotInvalid`] if its contents are not a snapshot.
pub fn load_snapshot_file(path: &Path) -> Result<Snapshot, CatalogError> {
    let raw = std::fs::read_to_string(path).map_err(|source| CatalogError::SnapshotUnreadable {
        path: path.display().to_string(),
        source,
    })?;
    let snapshot =
        serde_json::from_str::<Snapshot>(&raw).map_err(|source| CatalogError::SnapshotInvalid {
            path: path.display().to_string(),
            source,
        })?;
    tracing::info!(path = %path.display(), "loaded catalog snapshot from an explicit path");
    Ok(snapshot)
}

/// The snapshot the serve path starts from.
///
/// With no `override_path` this is the copy compiled into the binary, and
/// nothing else. The server speaks stdio and is launched by a client in
/// whatever directory that client happens to be in, so reading
/// `./data/catalog-snapshot.json` would let any repository the operator
/// happens to `cd` into replace the tool descriptions and schemas a model
/// reads and acts on. Overriding that is a decision the operator makes
/// explicitly with `--snapshot <PATH>`, not one a working directory makes for
/// them.
///
/// # Errors
///
/// Per [`embedded_snapshot`] or [`load_snapshot_file`]; an unusable explicit
/// path is fatal rather than a silent fall back to the embedded copy.
pub fn serve_snapshot(override_path: Option<&Path>) -> Result<Snapshot, CatalogError> {
    match override_path {
        Some(path) => load_snapshot_file(path),
        None => embedded_snapshot(),
    }
}

/// Load the working tree's snapshot, falling back to the embedded copy.
///
/// Only for the repository-facing subcommands (`print-catalog`), whose whole
/// purpose is to report on the file in the tree. The serve path must not use
/// this; see [`serve_snapshot`].
///
/// # Errors
///
/// Per [`embedded_snapshot`], which is the fallback.
pub fn load_working_tree_snapshot() -> Result<Snapshot, CatalogError> {
    let path = Path::new(SNAPSHOT_PATH);
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Snapshot>(&raw) {
            Ok(snapshot) => {
                tracing::debug!(path = %path.display(), "loaded catalog snapshot from disk");
                return Ok(snapshot);
            }
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "catalog snapshot on disk is unparseable; using the embedded copy"
            ),
        },
        Err(error) => tracing::debug!(
            path = %path.display(),
            %error,
            "no catalog snapshot on disk; using the embedded copy"
        ),
    }
    embedded_snapshot()
}

/// Log services present in the snapshot that the registry no longer pins.
///
/// Snapshot fallback is deliberately loud: a silently stale catalog would show
/// up to users as missing tools with no explanation.
pub fn warn_on_registry_drift(snapshot: &Snapshot) {
    for service in &snapshot.services {
        if registry::find(&service.service_id).is_none() {
            tracing::warn!(
                service = service.service_id,
                "snapshot holds a service missing from the endpoint registry; \
                 regenerate the snapshot with the `snapshot` subcommand"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    /// Build a `Tool` through serde; `rmcp::model::Tool` is `#[non_exhaustive]`,
    /// so it cannot be written as a struct literal from this crate.
    fn tool(name: &str, description: &str) -> Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": {} },
        }))
        .expect("test tool literal is valid")
    }

    fn service(service_id: &str, tools: &[(&str, &str)]) -> ServiceCatalog {
        ServiceCatalog {
            service_id: service_id.to_owned(),
            source: CatalogSource::Live,
            tools: tools
                .iter()
                .map(|(n, d)| NamespacedTool::new(service_id, tool(n, d)))
                .collect(),
        }
    }

    fn sample_catalog() -> Catalog {
        Catalog::new(vec![
            service(
                "run",
                &[
                    ("list_services", "List Cloud Run services in a project"),
                    ("get_service", "Get one Cloud Run service"),
                ],
            ),
            service(
                "bigquery",
                &[("list_datasets", "List BigQuery datasets in a project")],
            ),
        ])
        .expect("sample catalog satisfies namespacing invariants")
    }

    /// The catalog the binary actually ships, per acceptance criterion §5.2.
    fn committed_catalog() -> Catalog {
        serde_json::from_str::<Snapshot>(EMBEDDED_SNAPSHOT)
            .expect("embedded snapshot parses")
            .into_catalog()
            .expect("embedded snapshot satisfies namespacing invariants")
    }

    #[test]
    fn namespacing_joins_service_and_tool() {
        let namespaced = NamespacedTool::new("run", tool("list_services", "d"));
        assert_eq!(namespaced.namespaced_name, "run__list_services");
        assert_eq!(namespaced.service_id, "run");
        assert_eq!(namespaced.upstream_name(), "list_services");
    }

    #[test]
    fn committed_snapshot_has_no_duplicate_tool_names() {
        let catalog = committed_catalog();
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for service in &catalog.services {
            for entry in &service.tools {
                let previous = seen.insert(&entry.namespaced_name, &service.service_id);
                assert!(
                    previous.is_none(),
                    "`{}` served by both `{}` and `{}`",
                    entry.namespaced_name,
                    previous.unwrap_or_default(),
                    service.service_id
                );
            }
        }
    }

    #[test]
    fn committed_snapshot_tool_names_fit_the_length_limit() {
        for entry in committed_catalog().tools() {
            let len = entry.namespaced_name.chars().count();
            assert!(
                len <= MAX_TOOL_NAME_LEN,
                "`{}` is {len} chars, over the {MAX_TOOL_NAME_LEN}-char limit",
                entry.namespaced_name
            );
        }
    }

    #[test]
    fn committed_snapshot_services_are_all_registered() {
        for service in &committed_catalog().services {
            assert!(
                registry::find(&service.service_id).is_some(),
                "snapshot service `{}` is not in the endpoint registry",
                service.service_id
            );
        }
    }

    #[test]
    fn duplicate_namespaced_names_are_rejected() {
        let error = Catalog::new(vec![
            service("run", &[("list", "a")]),
            ServiceCatalog {
                service_id: "other".to_owned(),
                source: CatalogSource::Live,
                tools: vec![NamespacedTool {
                    namespaced_name: "run__list".to_owned(),
                    service_id: "other".to_owned(),
                    tool: tool("list", "b"),
                }],
            },
        ])
        .expect_err("colliding namespaced names must be rejected");

        assert!(
            matches!(error, CatalogError::DuplicateToolName { ref name, .. } if name == "run__list"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn overlong_namespaced_names_are_rejected() {
        let long = "t".repeat(MAX_TOOL_NAME_LEN);
        let error = Catalog::new(vec![service("run", &[(long.as_str(), "d")])])
            .expect_err("overlong namespaced names must be rejected");

        assert!(
            matches!(error, CatalogError::ToolNameTooLong { len, .. } if len == long.len() + 5),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn names_exactly_at_the_limit_are_accepted() {
        let name = "t".repeat(MAX_TOOL_NAME_LEN - "run__".len());
        let catalog = Catalog::new(vec![service("run", &[(name.as_str(), "d")])])
            .expect("a name of exactly the limit is valid");
        assert_eq!(
            catalog
                .tools()
                .next()
                .map(|t| t.namespaced_name.chars().count()),
            Some(MAX_TOOL_NAME_LEN)
        );
    }

    #[test]
    fn catalog_orders_services_and_tools_deterministically() {
        let catalog = sample_catalog();
        let service_ids: Vec<&str> = catalog
            .services
            .iter()
            .map(|s| s.service_id.as_str())
            .collect();
        assert_eq!(service_ids, ["bigquery", "run"]);

        let run = catalog.service("run").expect("run is present");
        let names: Vec<&str> = run
            .tools
            .iter()
            .map(|t| t.namespaced_name.as_str())
            .collect();
        assert_eq!(names, ["run__get_service", "run__list_services"]);
    }

    #[test]
    fn get_and_service_of_resolve_namespaced_names() {
        let catalog = sample_catalog();

        assert_eq!(
            catalog.get("run__list_services").map(|t| t.upstream_name()),
            Some("list_services")
        );
        assert_eq!(
            catalog
                .service_of("run__list_services")
                .map(|s| s.service_id.as_str()),
            Some("run")
        );

        assert!(catalog.get("run__missing").is_none());
        assert!(catalog.get("unknown__tool").is_none());
        assert!(
            catalog.get("list_services").is_none(),
            "unprefixed names must not resolve"
        );
        assert!(catalog.service_of("run__missing").is_none());
    }

    #[test]
    fn split_namespaced_rejects_malformed_names() {
        assert_eq!(split_namespaced("run__list"), Some(("run", "list")));
        // The first separator wins, so tool names may themselves contain `__`.
        assert_eq!(split_namespaced("run__a__b"), Some(("run", "a__b")));
        assert!(split_namespaced("run").is_none());
        assert!(split_namespaced("__list").is_none());
        assert!(split_namespaced("run__").is_none());
    }

    #[test]
    fn search_ranks_name_hits_above_description_hits() {
        // `beta_tool` matches only in its name, `alpha` only in its description.
        // `alpha` sorts first by name, so ordering here can only come from scoring.
        let catalog = Catalog::new(vec![service(
            "run",
            &[("alpha", "a beta thing"), ("beta_tool", "unrelated")],
        )])
        .expect("valid");

        let names: Vec<&str> = catalog
            .search("beta", None)
            .iter()
            .map(|t| t.namespaced_name.as_str())
            .collect();
        assert_eq!(names, ["run__beta_tool", "run__alpha"]);
    }

    #[test]
    fn search_matches_descriptions_when_names_do_not() {
        let catalog = sample_catalog();
        // "datasets" appears in one name; "cloud" only ever in descriptions.
        let names: Vec<&str> = catalog
            .search("cloud", None)
            .iter()
            .map(|t| t.namespaced_name.as_str())
            .collect();
        assert_eq!(names, ["run__get_service", "run__list_services"]);
    }

    #[test]
    fn search_is_conjunctive_over_tokens() {
        let catalog = sample_catalog();
        assert_eq!(catalog.search("list datasets", None).len(), 1);
        assert!(
            catalog.search("list nonexistentterm", None).is_empty(),
            "every token must match"
        );
    }

    #[test]
    fn search_honors_the_service_filter_and_case() {
        let catalog = sample_catalog();
        assert_eq!(catalog.search("LIST", Some("bigquery")).len(), 1);
        assert!(catalog.search("list", Some("nonexistent")).is_empty());
    }

    #[test]
    fn search_with_an_empty_query_returns_everything() {
        let catalog = sample_catalog();
        assert_eq!(catalog.search("", None).len(), catalog.tool_count());
        assert_eq!(catalog.search("   ", Some("run")).len(), 2);
    }

    #[test]
    fn search_order_is_deterministic_across_repeated_calls() {
        let catalog = sample_catalog();
        let first: Vec<&str> = catalog
            .search("list", None)
            .iter()
            .map(|t| t.namespaced_name.as_str())
            .collect();
        for _ in 0..8 {
            let again: Vec<&str> = catalog
                .search("list", None)
                .iter()
                .map(|t| t.namespaced_name.as_str())
                .collect();
            assert_eq!(first, again);
        }
    }

    #[test]
    fn exact_name_matches_outrank_substring_matches() {
        let catalog = Catalog::new(vec![service(
            "run",
            &[("list", "generic"), ("list_services", "list of things")],
        )])
        .expect("valid");
        let hits = catalog.search("list", None);
        assert_eq!(hits[0].upstream_name(), "list");
    }

    #[test]
    fn serving_a_snapshot_relabels_its_services_as_snapshot_sourced() {
        // The file records `live` because the tools were live when it was
        // generated; a process serving those bytes must not inherit that claim.
        let stored = sample_catalog();
        assert!(
            stored
                .services
                .iter()
                .all(|s| s.source == CatalogSource::Live)
        );

        let served = stored.marked_as(CatalogSource::Snapshot);
        assert!(
            served
                .services
                .iter()
                .all(|s| s.source == CatalogSource::Snapshot),
            "serving from disk must report snapshot provenance"
        );
        // Only the label changes.
        assert_eq!(served.tool_count(), stored.tool_count());
        assert_eq!(
            served.get("run__list_services").map(|t| &t.tool),
            stored.get("run__list_services").map(|t| &t.tool)
        );
    }

    #[test]
    fn restricted_to_keeps_only_the_named_endpoints() {
        let catalog = sample_catalog();
        let run = registry::find("run").expect("run is registered");
        let narrowed = catalog.restricted_to(&[run]);
        assert_eq!(narrowed.services.len(), 1);
        assert_eq!(narrowed.services[0].service_id, "run");
        assert!(narrowed.get("bigquery__list_datasets").is_none());
    }

    #[test]
    fn drift_reports_names_and_schemas_but_ignores_descriptions() {
        let before = Catalog::new(vec![service(
            "run",
            &[("stable", "original text"), ("removed_later", "d")],
        )])
        .expect("valid");
        let after = Catalog::new(vec![service(
            "run",
            // `stable` keeps its name and schema but changes description text;
            // this is the cloudcli replica-variance case and must NOT be drift.
            &[
                ("stable", "COMPLETELY DIFFERENT TEXT"),
                ("added_later", "d"),
            ],
        )])
        .expect("valid");

        let drift = before.drift_from(&after);
        assert_eq!(drift.added, ["run__added_later"]);
        assert_eq!(drift.removed, ["run__removed_later"]);
        assert!(
            drift.schema_changed.is_empty(),
            "description-only changes must not count as drift"
        );
        assert!(!drift.is_empty());
        assert!(before.drift_from(&before).is_empty());
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let catalog = sample_catalog();
        let snapshot = catalog.to_snapshot("2026-08-19T00:00:00Z".to_owned());

        let encoded = serde_json::to_string(&snapshot).expect("snapshot serializes");
        let decoded: Snapshot = serde_json::from_str(&encoded).expect("snapshot parses");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.generated_at, "2026-08-19T00:00:00Z");
        assert_eq!(decoded.into_catalog().expect("valid"), catalog);
    }

    #[test]
    fn catalog_source_survives_the_round_trip() {
        let snapshot = Snapshot {
            generated_at: "2026-08-19T00:00:00Z".to_owned(),
            services: vec![ServiceCatalog {
                service_id: "run".to_owned(),
                source: CatalogSource::Snapshot,
                tools: vec![],
            }],
        };
        let encoded = serde_json::to_string(&snapshot).expect("serializes");
        assert!(encoded.contains(r#""source":"snapshot""#));
        let decoded: Snapshot = serde_json::from_str(&encoded).expect("parses");
        assert_eq!(decoded.services[0].source, CatalogSource::Snapshot);
    }

    /// Directory for this test process's snapshot fixtures.
    fn fixture_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mcp-google-service-catalog-tests-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Write `contents` to a fixture file and return its path.
    fn write_fixture(name: &str, contents: &str) -> std::path::PathBuf {
        let path = fixture_dir().join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    /// A one-service snapshot, distinguishable from the embedded copy.
    fn fixture_snapshot_json() -> String {
        let snapshot = Snapshot {
            generated_at: "2026-08-19T00:00:00Z".to_owned(),
            services: vec![ServiceCatalog {
                service_id: "run".to_owned(),
                source: CatalogSource::Live,
                tools: vec![],
            }],
        };
        serde_json::to_string(&snapshot).expect("a constructed snapshot always serializes")
    }

    #[test]
    fn serve_defaults_to_the_embedded_snapshot() {
        // Not "prefers": the default path has no disk branch at all, which is
        // what keeps a working directory chosen by the client from deciding
        // which tool descriptions a model reads.
        let served = serve_snapshot(None).expect("the embedded snapshot always parses");
        assert_eq!(served, embedded_snapshot().expect("embedded parses"));
        assert!(
            served.services.len() > 1,
            "sanity: the embedded snapshot carries the real registry"
        );
    }

    /// The property the default exists for, proven rather than asserted by
    /// construction: a `data/catalog-snapshot.json` sitting in the process's
    /// working directory must not reach the serve path.
    ///
    /// This server is spawned by an MCP client, in whatever directory that
    /// client was started in. Tool descriptions and schemas are instructions a
    /// model reads and acts on, so a repository the operator merely happened
    /// to open must not be able to supply them.
    ///
    /// # Test-runner assumption
    ///
    /// Changes the process working directory, which is global state. The
    /// mandated runner (`cargo nextest`) executes each test in its own
    /// process, so no sibling test observes it; the directory is restored
    /// before any assertion can unwind the test.
    #[test]
    fn serve_ignores_a_snapshot_planted_in_the_working_directory() {
        let planted = fixture_dir().join("planted-working-tree");
        std::fs::create_dir_all(planted.join("data")).expect("fixture tree");
        std::fs::write(planted.join(SNAPSHOT_PATH), fixture_snapshot_json())
            .expect("plant a snapshot where a working-tree read would find it");

        let original = std::env::current_dir().expect("the test process has a working directory");
        std::env::set_current_dir(&planted).expect("enter the planted directory");
        let served = serve_snapshot(None);
        let working_tree = load_working_tree_snapshot();
        std::env::set_current_dir(&original).expect("restore the working directory");

        let working_tree = working_tree.expect("the planted file is a valid snapshot");
        assert_eq!(
            working_tree.services.len(),
            1,
            "sanity: a working-tree read does find the planted file, so the \
             comparison below is meaningful"
        );

        let served = served.expect("the embedded snapshot always parses");
        assert_eq!(
            served,
            embedded_snapshot().expect("embedded parses"),
            "the serve path must ignore a snapshot planted in the working \
             directory and use only the copy compiled into the binary"
        );
    }

    #[test]
    fn an_explicit_path_overrides_the_embedded_snapshot() {
        let path = write_fixture("explicit-catalog-snapshot.json", &fixture_snapshot_json());

        let served = serve_snapshot(Some(&path)).expect("an explicit, valid snapshot loads");
        assert_eq!(served.generated_at, "2026-08-19T00:00:00Z");
        assert_eq!(
            served.services.len(),
            1,
            "the operator's file must be what is served, not the embedded copy"
        );

        std::fs::remove_file(&path).expect("clean up fixture");
    }

    #[test]
    fn a_corrupt_explicit_path_is_fatal_rather_than_silently_replaced() {
        let path = write_fixture("corrupt-catalog-snapshot.json", "{ not json");
        let error = serve_snapshot(Some(&path))
            .expect_err("a named snapshot that cannot be parsed must not fall back");
        assert!(
            matches!(error, CatalogError::SnapshotInvalid { .. }),
            "expected an explicit parse failure, got: {error}"
        );

        let missing = Path::new("/nonexistent/mcp-google-service/catalog-snapshot.json");
        let error = serve_snapshot(Some(missing))
            .expect_err("a named snapshot that does not exist must not fall back");
        assert!(
            matches!(error, CatalogError::SnapshotUnreadable { .. }),
            "expected an explicit read failure, got: {error}"
        );

        std::fs::remove_file(&path).expect("clean up fixture");
    }

    #[test]
    fn the_working_tree_loader_still_falls_back_to_the_embedded_copy() {
        // `print-catalog` reports on the repository it is run in, so its loader
        // keeps the tolerant behaviour the serve path gave up.
        let loaded = load_working_tree_snapshot().expect("the loader always yields a snapshot");
        assert!(
            !loaded.services.is_empty(),
            "either the working tree's copy or the embedded one must be returned"
        );
    }
}
