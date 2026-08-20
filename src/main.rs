//! Unified Google Cloud MCP aggregator: one stdio MCP server fronting the
//! per-service `https://{service}.googleapis.com/mcp` endpoints.

use std::{io::Write, path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};

use mcp_google_service::{
    archive, auth, catalog,
    config::{Config, ExposeMode},
    proxy, registry,
    server::{self, BackgroundStartup, CredentialState, Readiness},
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

    /// Resolve credentials and the enabled-API list before serving; fail fast.
    ///
    /// By default the server answers `initialize` and `tools/list` from the
    /// snapshot at once and acquires credentials and the enabled-API list in
    /// the background; a problem with either shows in `list_services` and on
    /// the first `call`. This flag restores the earlier behaviour for operators
    /// who would rather the process exit than serve with broken credentials.
    /// `--expose flat` implies it, because its tool list is fixed at
    /// `initialize` and so has to be final before anything is served.
    #[arg(long)]
    strict_startup: bool,
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

        /// Re-encode this snapshot JSON instead of fanning out.
        ///
        /// Reads the file, keeps its `generated_at`, and emits the same
        /// outputs a live fan-out would. This is how the committed archive is
        /// regenerated from the committed JSON without touching the network,
        /// so the two artifacts can never legitimately disagree.
        #[arg(long, value_name = "PATH", conflicts_with = "allow_partial")]
        from: Option<PathBuf>,
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
        Some(Command::Snapshot {
            out,
            allow_partial,
            from,
        }) => run_snapshot(out, allow_partial, from).await,
        Some(Command::PrintCatalog) => run_print_catalog(),
    }
}

/// Run the stdio MCP server.
///
/// The catalog is served from the snapshot immediately, and everything that
/// needs the network -- acquiring credentials, asking Service Usage which APIs
/// are enabled, refreshing the catalog from the upstreams -- runs after the
/// client's `initialize` has been answered. In two-tier mode the four exposed
/// tools never change, so narrowing or refreshing the catalog underneath needs
/// no `listChanged` notification; flat mode's tool list *is* the catalog and
/// the client has no way to be told it moved, so flat mode resolves the
/// exposed set before serving and keeps serving that startup catalog.
async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    tracing::debug!(
        project = ?args.project,
        only = ?args.only,
        exclude = ?args.exclude,
        expose = ?args.expose,
        snapshot = ?args.snapshot,
        strict_startup = args.strict_startup,
        "parsed command line"
    );

    let cfg = Config::resolve(
        args.project,
        args.only,
        args.exclude,
        args.expose,
        args.strict_startup,
    )?;
    let http = proxy::shared_http_client().context("building the shared HTTP client")?;
    let catalog = catalog::serve_catalog(args.snapshot.as_deref())?;
    catalog::warn_on_registry_drift(&catalog.services);

    // Two ways to a served catalog. Strict resolves credentials and enablement
    // first and fails fast, so nothing is served with broken credentials. The
    // default serves the configured selection at once with credentials
    // discovered on first use, and resolves both behind the handshake.
    let (auth, state, exposed, background) = if cfg.strict_startup {
        tracing::info!(
            reason = match cfg.expose {
                ExposeMode::Flat => {
                    "`--expose flat` fixes the tool list at `initialize`, so the \
                     exposed set has to be final before anything is served"
                }
                ExposeMode::TwoTier => "`--strict-startup`",
            },
            "resolving credentials and enablement before serving; a credential \
             failure is fatal here"
        );
        let auth = Arc::new(auth::AuthContext::new(&cfg).await?);
        // Discovery is not acquisition. `AuthContext::new` only finds the
        // credential chain; it mints nothing, so on its own it cannot tell a
        // usable credential from a discoverable one. `--only` then skips
        // Service Usage, which is the one call that would otherwise have
        // forced a token, so without this the strict path could serve while
        // reporting `credentials: ready` having never held a token -- a false
        // ready being strictly worse than no readiness at all, and the exact
        // failure this mode exists to prevent. Fatal here by design: that is
        // what `--strict-startup` asks for.
        auth.apply(&mut reqwest::header::HeaderMap::new())
            .await
            .context("acquiring Google credentials before serving (`--strict-startup`)")?;
        let exposure = server::resolve_enablement(&cfg, &auth, &http).await;
        let state = server::assemble_serve_catalog(catalog, &exposure.endpoints)?;
        state.publish_readiness(Readiness {
            credentials: CredentialState::Ready,
            enablement: exposure.enablement,
        });
        (auth, state, exposure.endpoints, None)
    } else {
        let auth = Arc::new(auth::AuthContext::new_lazy(&cfg)?);
        let configured = server::configured_endpoints(&cfg);
        let state = server::assemble_serve_catalog(catalog, &configured)?;
        let background = BackgroundStartup {
            state: state.clone(),
            auth: Arc::clone(&auth),
            http: http.clone(),
            config: cfg.clone(),
        };
        (auth, state, configured, Some(background))
    };
    let startup_note = match (cfg.strict_startup, cfg.expose) {
        (true, ExposeMode::Flat) => {
            "credentials and enablement resolved before serving; tool list pinned (`--expose flat`)"
        }
        (true, ExposeMode::TwoTier) => {
            "credentials and enablement resolved before serving (`--strict-startup`)"
        }
        (false, _) => "credentials, enablement and the live refresh resolving in the background",
    };
    tracing::info!(
        services = state.startup().services.len(),
        tools = state.startup().tool_count(),
        "serving from snapshot; {startup_note}"
    );

    // Exposure is enforced by the catalog, which both surfaces consult before
    // dispatching, so the proxy can route to every configured endpoint and
    // the background narrowing only has to swap the catalog.
    let handler = server::GoogleMcpServer::new(
        state.clone(),
        Arc::new(proxy::Proxy::from_endpoints(auth, http.clone(), &exposed)),
        cfg.expose,
    );
    let running = handler
        .serve(stdio())
        .await
        .context("starting the stdio MCP server")?;

    // `serve` returns once the client's `initialize` has been answered. Only
    // now does anything reach for the network, so neither credential
    // discovery nor the refresh fan-out competes with the handshake.
    match (background, cfg.expose) {
        (Some(background), _) => {
            tokio::spawn(background.run());
        }
        (None, ExposeMode::TwoTier) => {
            tokio::spawn(server::refresh_live_catalog(state.live(), exposed, http));
        }
        // Nothing would read the result: the flat surface is pinned to the
        // startup catalog, so refreshing would spend a fan-out to update a
        // catalog no request consults, and log that it swapped in.
        (None, ExposeMode::Flat) => tracing::debug!(
            "not refreshing the catalog: `--expose flat` pins the tool list at \
             startup, so a refresh would be discarded"
        ),
    }

    running.waiting().await.context("serving stdio MCP")?;
    Ok(())
}

/// Fetch every registered endpoint live and emit the snapshot JSON.
///
/// No pruning and no fallback: a snapshot must record exactly what the
/// upstreams answered, so a host that fails is reported rather than papered
/// over with its previous contents, and a short fan-out fails the command
/// unless `--allow-partial` says a partial capture is wanted.
async fn run_snapshot(
    out: Option<PathBuf>,
    allow_partial: bool,
    from: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Re-encode mode: the catalog and its timestamp come from the named file,
    // no byte leaves the machine, and the outputs are what a fan-out that had
    // observed the same catalog at the same instant would have written.
    if let Some(path) = from {
        let snapshot = catalog::load_snapshot_file(&path)?;
        let generated_at = snapshot.generated_at.clone();
        let catalog = snapshot
            .into_catalog()
            .context("validating the snapshot named by --from")?;
        return write_snapshot_outputs(&catalog, generated_at, out);
    }

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
    write_snapshot_outputs(&catalog, generated_at, out)
}

/// Write a catalog's snapshot JSON and, when a path is named, its archive.
///
/// The archive lands beside the JSON with a `.bin` extension. It is derived
/// from exactly the snapshot being written -- same catalog, same timestamp --
/// so committing the pair keeps `archive::tests` able to hold them identical.
/// Stdout mode emits JSON only: a terminal is no place for a binary artifact,
/// and an archive nobody can commit has no consumer.
fn write_snapshot_outputs(
    catalog: &catalog::Catalog,
    generated_at: String,
    out: Option<PathBuf>,
) -> anyhow::Result<()> {
    let snapshot = catalog.to_snapshot(generated_at);
    let json = snapshot.to_json()?;

    match out {
        Some(path) => {
            std::fs::write(&path, &json)
                .with_context(|| format!("writing snapshot to {}", path.display()))?;
            tracing::info!(path = %path.display(), bytes = json.len(), "snapshot written");

            let archive_path = path.with_extension("bin");
            let archive = archive::build(&snapshot).context("building the catalog archive")?;
            std::fs::write(&archive_path, &archive)
                .with_context(|| format!("writing archive to {}", archive_path.display()))?;
            tracing::info!(
                path = %archive_path.display(),
                bytes = archive.len(),
                "catalog archive written"
            );
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
    catalog::warn_on_registry_drift(&snapshot.services);

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
