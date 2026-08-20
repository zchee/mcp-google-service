//! Unified Google Cloud MCP aggregator: one stdio MCP server fronting the
//! per-service `https://{service}.googleapis.com/mcp` endpoints.

use std::{io::Write, path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};

use mcp_google_service::{
    auth, catalog,
    config::{Config, ExposeMode},
    proxy, prune,
    registry::{self, Endpoint},
    server,
};

/// Aggregates Google Cloud remote MCP endpoints behind one stdio server.
//
// The serve flags live on `ServeArgs`, flattened here as well as onto the
// `serve` subcommand: `mcp-google-service --project p` (no subcommand) keeps
// working, while `snapshot --help` and `print-catalog --help` stop advertising
// flags they ignore, which is what they did while these were `global = true`.
// `args_conflicts_with_subcommands` keeps the two copies from being combined.
#[derive(Debug, Parser)]
#[command(name = "mcp-google-service", version, about)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(flatten)]
    serve: ServeArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Options that only mean something while serving.
#[derive(Debug, Clone, Args)]
struct ServeArgs {
    /// Quota project for `x-goog-user-project` and Service Usage pruning.
    ///
    /// Also read from `GOOGLE_MCP_QUOTA_PROJECT`, then `GOOGLE_CLOUD_PROJECT`,
    /// then the ADC file's `quota_project_id`.
    #[arg(long)]
    project: Option<String>,

    /// Only expose these service ids (comma-separated); skips enablement pruning.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,

    /// Never expose these service ids (comma-separated); wins over `--only`.
    #[arg(long, value_delimiter = ',')]
    exclude: Vec<String>,

    /// Tool-surface mode.
    #[arg(long, value_enum, default_value_t = ExposeMode::TwoTier)]
    expose: ExposeMode,

    /// Serve tool metadata from this snapshot file instead of the embedded one.
    ///
    /// Off by default on purpose: this server is launched by a client in
    /// whatever directory that client happens to be in, and tool descriptions
    /// are instructions a model acts on, so which file supplies them is a
    /// decision the operator makes rather than one the working directory makes
    /// for them. An unreadable or invalid path here is fatal.
    #[arg(long, value_name = "PATH")]
    snapshot: Option<PathBuf>,
}

/// Top-level subcommands; `serve` is the default when none is given.
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the stdio MCP server (default).
    Serve(ServeArgs),
    /// Fetch the upstream catalog and emit a snapshot as JSON.
    ///
    /// Discovery needs no credentials, so this runs against an empty
    /// environment. Writing through `--out` is preferred over a shell
    /// redirect, which fails under `noclobber` when the file already exists.
    Snapshot {
        /// Write to this path instead of stdout (e.g. `data/catalog-snapshot.json`).
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Emit the snapshot even when some endpoints could not be reached.
        ///
        /// Without this a short fan-out exits non-zero: a snapshot silently
        /// missing services becomes a binary that silently cannot offer their
        /// tools.
        #[arg(long)]
        allow_partial: bool,
    },
    /// Print the snapshot's per-service tool counts to stdout.
    PrintCatalog,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    for endpoint in registry::ENDPOINTS {
        tracing::debug!(
            service = endpoint.service_id,
            host = endpoint.host,
            api = endpoint.api_name,
            "registered endpoint"
        );
    }

    match cli.command {
        None => run_serve(cli.serve).await,
        Some(Command::Serve(args)) => run_serve(args).await,
        Some(Command::Snapshot { out, allow_partial }) => run_snapshot(out, allow_partial).await,
        Some(Command::PrintCatalog) => run_print_catalog(),
    }
}

/// Run the stdio MCP server.
///
/// The catalog is served from the snapshot immediately and refreshed once in
/// the background, so time-to-first-tool-response does not wait on a 47-host
/// fan-out. In two-tier mode the four exposed tools never change, so swapping
/// the catalog underneath needs no `listChanged` notification; flat mode
/// therefore keeps serving the startup catalog, since its tool list *is* the
/// catalog and the client has no way to be told it moved.
async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    tracing::debug!(
        project = ?args.project,
        only = ?args.only,
        exclude = ?args.exclude,
        expose = ?args.expose,
        snapshot = ?args.snapshot,
        "parsed command line"
    );

    let cfg = Config::resolve(args.project, args.only, args.exclude, args.expose)?;
    let auth = Arc::new(auth::AuthContext::new(&cfg).await?);
    let http = proxy::shared_http_client().context("building the shared HTTP client")?;

    let exposed = resolve_exposed_endpoints(&cfg, &auth, &http).await;
    tracing::info!(
        services = exposed.len(),
        of = registry::ENDPOINTS.len(),
        "services selected for exposure"
    );

    let snapshot = catalog::serve_snapshot(args.snapshot.as_deref())?;
    catalog::warn_on_registry_drift(&snapshot);
    let state = server::assemble_serve_catalog(snapshot, &exposed)?;
    let refresh_note = match cfg.expose {
        ExposeMode::TwoTier => "live refresh running in the background",
        ExposeMode::Flat => "tool list pinned at startup (`--expose flat`)",
    };
    tracing::info!(
        services = state.startup().services.len(),
        tools = state.startup().tool_count(),
        "serving from snapshot; {refresh_note}"
    );

    // The refresh task takes ownership of the endpoint list; dispatch keeps its own copy.
    let exposed_routes = exposed.clone();
    match cfg.expose {
        ExposeMode::TwoTier => spawn_catalog_refresh(state.live(), exposed, http.clone()),
        // Nothing would read the result: the flat surface is pinned to the
        // startup catalog, so refreshing would spend a 47-host fan-out to
        // update a catalog no request consults, and log that it swapped in.
        ExposeMode::Flat => tracing::debug!(
            "not refreshing the catalog: `--expose flat` pins the tool list at \
             startup, so a refresh would be discarded"
        ),
    }

    let handler = server::GoogleMcpServer::new(
        state,
        Arc::new(proxy::Proxy::from_endpoints(auth, http, &exposed_routes)),
        cfg.expose,
    );
    let running = handler
        .serve(stdio())
        .await
        .context("starting the stdio MCP server")?;
    running.waiting().await.context("serving stdio MCP")?;
    Ok(())
}

/// Decide which endpoints to expose, degrading loudly when pruning fails.
///
/// A Service Usage failure must not take the server down: it warns, names the
/// cause, and falls back to the configured selection (per plan P3).
async fn resolve_exposed_endpoints(
    cfg: &Config,
    auth: &auth::AuthContext,
    http: &reqwest::Client,
) -> Vec<&'static Endpoint> {
    let enabled = match prune::enabled_services(auth, &cfg.quota_project, http).await {
        Ok(enabled) => {
            tracing::info!(
                enabled = enabled.len(),
                project = %cfg.quota_project,
                "Service Usage reported enabled APIs"
            );
            Some(enabled)
        }
        Err(error) => {
            tracing::warn!(
                project = %cfg.quota_project,
                cause = %error,
                "could not determine enabled APIs; exposing the configured selection unpruned"
            );
            None
        }
    };
    prune::select_services(
        registry::ENDPOINTS,
        enabled.as_ref(),
        &cfg.only,
        &cfg.exclude,
    )
}

/// Refresh the catalog from the upstreams and swap it in when it lands.
fn spawn_catalog_refresh(
    shared: server::SharedCatalog,
    exposed: Vec<&'static Endpoint>,
    http: reqwest::Client,
) {
    tokio::spawn(async move {
        let fallback = Arc::clone(&*shared.read().await);
        match catalog::Catalog::build_live(exposed, &http, Some(&fallback)).await {
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
    });
}

/// Fetch every registered endpoint live and emit the snapshot JSON.
///
/// No pruning and no fallback: a snapshot must record exactly what the
/// upstreams answered, so a host that fails is reported rather than papered
/// over with its previous contents, and a short fan-out fails the command
/// unless `--allow-partial` says a partial capture is wanted.
async fn run_snapshot(out: Option<PathBuf>, allow_partial: bool) -> anyhow::Result<()> {
    let http = proxy::shared_http_client().context("building the shared HTTP client")?;
    let catalog = catalog::Catalog::build_live(registry::ENDPOINTS, &http, None).await?;

    let reached = catalog.services.len();
    let expected = registry::ENDPOINTS.len();
    if reached < expected {
        // A partial snapshot compiled into the binary is indistinguishable
        // from a registry that never had those services, so a short fan-out
        // fails the command unless the operator says otherwise.
        if !allow_partial {
            bail!(
                "snapshot reached {reached} of {expected} endpoints; {} did not answer \
                 (see the warnings above). Re-run when they are reachable, or pass \
                 `--allow-partial` to write the snapshot anyway",
                expected - reached
            );
        }
        tracing::warn!(
            reached,
            expected,
            "snapshot is missing {} endpoint(s); writing it anyway because \
             --allow-partial was given",
            expected - reached
        );
    }
    tracing::info!(
        services = reached,
        tools = catalog.tool_count(),
        "catalog fan-out complete"
    );

    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let json = catalog.to_snapshot_json(generated_at)?;

    match out {
        Some(path) => {
            std::fs::write(&path, &json)
                .with_context(|| format!("writing snapshot to {}", path.display()))?;
            tracing::info!(path = %path.display(), bytes = json.len(), "snapshot written");
        }
        None => std::io::stdout()
            .write_all(json.as_bytes())
            .context("writing snapshot to stdout")?,
    }
    Ok(())
}

/// Print the committed snapshot as a service/tool-count/source table.
fn run_print_catalog() -> anyhow::Result<()> {
    // Repository-facing: this reports on the working tree's snapshot when
    // there is one, which is the file an operator is about to review or
    // regenerate. The serve path deliberately does not share that behaviour.
    let snapshot = catalog::load_working_tree_snapshot()?;
    catalog::warn_on_registry_drift(&snapshot);

    let generated_at = snapshot.generated_at.clone();
    let catalog = snapshot.into_catalog()?;

    const ID_HEADER: &str = "SERVICE";
    const COUNT_HEADER: &str = "TOOLS";
    let id_width = catalog
        .services
        .iter()
        .map(|s| s.service_id.len())
        .chain(std::iter::once(ID_HEADER.len()))
        .max()
        .unwrap_or(ID_HEADER.len());

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    writeln!(out, "generated_at: {generated_at}")?;
    writeln!(out)?;
    writeln!(out, "{ID_HEADER:<id_width$}  {COUNT_HEADER:>5}  SOURCE")?;
    for service in &catalog.services {
        writeln!(
            out,
            "{:<id_width$}  {:>5}  {}",
            service.service_id,
            service.tools.len(),
            service.source
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "{:<id_width$}  {:>5}",
        format!("{} services", catalog.services.len()),
        catalog.tool_count()
    )?;
    Ok(())
}

/// Initialize the tracing subscriber on stderr (stdout carries MCP frames).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
