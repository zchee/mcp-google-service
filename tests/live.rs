//! Live tier: real Google endpoints, real Application Default Credentials.
//!
//! Every test here skips cleanly unless `MCP_GOOGLE_LIVE=1` is set, so the
//! default `cargo nextest run` stays hermetic. No test is marked `#[ignore]`:
//! they run, report why they are inert, and pass. That way an accidental
//! change to the gate shows up as a failure rather than as silently skipped
//! coverage.
//!
//! Requirements when enabled:
//!
//! * `MCP_GOOGLE_LIVE=1`
//! * `GOOGLE_MCP_QUOTA_PROJECT=<project>` -- never hardcoded here, because the
//!   project is the operator's, not this repository's.
//! * Application Default Credentials (`gcloud auth application-default login`).
//!
//! Only read-only tools are exercised, per plan section 1 (non-goals).
//!
//! Criterion mapping: section 5.7 dispatch and the latency targets as amended
//! by ledger deviation #5a.

use std::time::{Duration, Instant};

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::child_process::TokioChildProcess,
};
use serde_json::{Value, json};

use mcp_google_service::{catalog, proxy::shared_http_client, registry};

/// Environment variable enabling this tier.
const LIVE_GATE: &str = "MCP_GOOGLE_LIVE";
/// Environment variable naming the quota project.
const PROJECT_VAR: &str = "GOOGLE_MCP_QUOTA_PROJECT";

/// Latency budget from initialize to the first tool response (ledger #5a).
const FIRST_RESPONSE_BUDGET: Duration = Duration::from_millis(100);
/// Latency budget for parsing the committed snapshot (ledger #5a).
const SNAPSHOT_PARSE_BUDGET: Duration = Duration::from_millis(500);
/// Latency budget from process start to a server able to answer (ledger #5a).
const READY_BUDGET: Duration = Duration::from_secs(3);
/// Latency budget for one background catalog refresh fan-out (ledger #5a).
const REFRESH_BUDGET: Duration = Duration::from_secs(10);

/// The quota project to test against, or `None` when the tier is disabled.
///
/// Returns `None` (and says so) rather than failing when the gate is unset.
/// When the gate *is* set, a missing project is a hard error: silently
/// skipping would misreport the tier as having run.
fn live_project() -> Option<String> {
    if std::env::var(LIVE_GATE).ok().as_deref() != Some("1") {
        eprintln!("live tier inert: set {LIVE_GATE}=1 and {PROJECT_VAR}=<project> to run it");
        return None;
    }
    let project = std::env::var(PROJECT_VAR).unwrap_or_else(|_| {
        panic!("{LIVE_GATE}=1 requires {PROJECT_VAR} to name the quota project to bill and prune against")
    });
    assert!(
        !project.trim().is_empty(),
        "{PROJECT_VAR} must not be empty"
    );
    Some(project)
}

/// A live MCP session against the compiled binary, plus how long it took to
/// become ready.
struct LiveSession {
    client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    /// Process spawn through completed `initialize`.
    ready_after: Duration,
}

impl LiveSession {
    /// Spawn the real binary and complete the MCP handshake over stdio.
    async fn start(project: &str) -> Self {
        // `CARGO_BIN_EXE_*` points at the binary cargo just built, so this
        // exercises the shipped artifact rather than a re-assembled server.
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-google-service"));
        command.arg("--project").arg(project);

        let started = Instant::now();
        let transport = TokioChildProcess::new(command)
            .expect("the built binary must be spawnable for the live tier");
        let client = ()
            .serve(transport)
            .await
            .expect("the binary must complete the MCP initialize handshake");
        let ready_after = started.elapsed();

        Self {
            client,
            ready_after,
        }
    }

    /// Invoke a two-tier meta-tool by name.
    async fn call(&self, name: &str, arguments: Value) -> CallToolResult {
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = arguments.as_object().cloned();
        self.client.call_tool(params).await.unwrap_or_else(|error| {
            panic!("live call to `{name}` failed at the protocol level: {error}")
        })
    }

    /// Dispatch a namespaced upstream tool through the two-tier `call` tool.
    async fn dispatch(&self, target: &str, arguments: Value) -> CallToolResult {
        self.call("call", json!({ "name": target, "arguments": arguments }))
            .await
    }

    async fn shutdown(self) {
        let _ = self.client.cancel().await;
    }
}

/// Concatenated text of a tool result.
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assert a live dispatch succeeded, reporting the remediation text if not.
///
/// A failure here is deliberately loud and unmodified: per the task brief, an
/// upstream that is disabled on the operator's project is a finding to report,
/// not a reason to weaken the assertion.
fn assert_live_dispatch_ok(target: &str, result: &CallToolResult) {
    let text = result_text(result);
    assert_ne!(
        result.is_error,
        Some(true),
        "live dispatch of `{target}` returned an error result. This is reported \
         verbatim rather than tolerated, because enabling an API on the \
         operator's project is their decision, not this test's:\n{text}"
    );
    assert!(
        !text.trim().is_empty(),
        "live dispatch of `{target}` returned an empty result body"
    );
}

// ---------------------------------------------------------------------------
// Section 5.7 -- live dispatch
// ---------------------------------------------------------------------------

/// `run__list_services` must return a non-error result against the real API.
#[tokio::test]
async fn live_run_list_services_dispatches_without_error() {
    let Some(project) = live_project() else {
        return;
    };
    let session = LiveSession::start(&project).await;

    let result = session
        .dispatch(
            "run__list_services",
            // Both arguments are required by the upstream's own schema.
            json!({ "project": project, "region": "us-central1" }),
        )
        .await;
    assert_live_dispatch_ok("run__list_services", &result);
    eprintln!(
        "live run__list_services returned {} content block(s)",
        result.content.len()
    );

    session.shutdown().await;
}

/// `developerknowledge__search_documents` must return a non-error result.
#[tokio::test]
async fn live_developerknowledge_search_documents_dispatches_without_error() {
    let Some(project) = live_project() else {
        return;
    };
    let session = LiveSession::start(&project).await;

    let result = session
        .dispatch(
            "developerknowledge__search_documents",
            json!({ "query": "Cloud Run deploy a container" }),
        )
        .await;
    assert_live_dispatch_ok("developerknowledge__search_documents", &result);
    eprintln!(
        "live developerknowledge__search_documents returned {} content block(s)",
        result.content.len()
    );

    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// Section 5.7 / ledger #5a -- latency targets
// ---------------------------------------------------------------------------

/// Start-to-ready and initialize-to-first-response must meet their budgets.
///
/// Both are measured against the real binary: `ready_after` covers process
/// spawn, ADC acquisition, Service Usage pruning and snapshot load, and the
/// first response is served from the in-memory snapshot while the live
/// refresh is still running in the background.
#[tokio::test]
async fn live_startup_and_first_tool_response_meet_their_latency_budgets() {
    let Some(project) = live_project() else {
        return;
    };
    let session = LiveSession::start(&project).await;

    let started = Instant::now();
    let listed = session.call("list_services", json!({})).await;
    let first_response = started.elapsed();

    assert_ne!(
        listed.is_error,
        Some(true),
        "list_services must answer from the snapshot immediately: {}",
        result_text(&listed)
    );

    // The first response is served before any live fetch can have landed, so
    // every service must say so. This is the end-to-end witness for the
    // provenance relabel in the serve assembly: if it were dropped, the binary
    // would report month-old tool definitions as freshly fetched.
    let payload: Value =
        serde_json::from_str(&result_text(&listed)).expect("list_services returns a JSON payload");
    let services = payload["services"]
        .as_array()
        .expect("list_services reports a services array");
    assert!(
        !services.is_empty(),
        "the live tier must expose at least one service, or the provenance \
         assertion below is vacuous: {payload}"
    );
    for service in services {
        assert_eq!(
            service["source"],
            json!("snapshot"),
            "before the background refresh lands, every service is being \
             served from the snapshot and must report it: {payload}"
        );
    }

    eprintln!("process start -> ready: {:?}", session.ready_after);
    eprintln!("initialize -> first tool response: {first_response:?}");

    assert!(
        session.ready_after < READY_BUDGET,
        "process start to ready took {:?}, over the {READY_BUDGET:?} budget",
        session.ready_after
    );
    assert!(
        first_response < FIRST_RESPONSE_BUDGET,
        "first tool response took {first_response:?}, over the \
         {FIRST_RESPONSE_BUDGET:?} budget; the snapshot-first startup path is \
         what keeps this off the catalog fan-out"
    );

    session.shutdown().await;
}

/// A warm dispatch reuses its session and answers faster than the cold one.
///
/// P3 caches the upstream MCP session, so only the first dispatch to a service
/// pays the `initialize` handshake; subsequent dispatches to the same service
/// skip it. Against real Google latency the saving is one round trip, which
/// varies run to run, so this reports both timings and the delta rather than
/// asserting a threshold that would be flaky -- the in-process
/// `a_second_dispatch_reuses_the_session_and_does_not_reinitialize` is what
/// proves the handshake is gone by counting it. Here the only assertions are
/// that both dispatches succeed and, loosely, that the warm one did not come
/// out dramatically slower, which would signal the cache is not being hit.
#[tokio::test]
async fn live_second_dispatch_reuses_the_session() {
    let Some(project) = live_project() else {
        return;
    };
    let session = LiveSession::start(&project).await;

    let args = json!({ "project": project, "region": "us-central1" });

    let cold_start = Instant::now();
    let cold = session.dispatch("run__list_services", args.clone()).await;
    let cold_elapsed = cold_start.elapsed();
    assert_live_dispatch_ok("run__list_services (cold)", &cold);

    let warm_start = Instant::now();
    let warm = session.dispatch("run__list_services", args).await;
    let warm_elapsed = warm_start.elapsed();
    assert_live_dispatch_ok("run__list_services (warm)", &warm);

    eprintln!("dispatch cold (with handshake): {cold_elapsed:?}");
    eprintln!("dispatch warm (session reused): {warm_elapsed:?}");
    if let Some(saved) = cold_elapsed.checked_sub(warm_elapsed) {
        eprintln!("warm saved: {saved:?} (one avoided initialize round trip)");
    }

    // Not a hard latency budget -- Google's own latency dominates and varies --
    // but a warm dispatch several times slower than the cold one would mean the
    // session is being rebuilt every call, which is the regression this guards.
    assert!(
        warm_elapsed < cold_elapsed * 3 + Duration::from_millis(500),
        "warm dispatch ({warm_elapsed:?}) was far slower than cold ({cold_elapsed:?}); \
         the session cache does not appear to be reused"
    );

    session.shutdown().await;
}

/// Parsing the committed snapshot must stay within its budget.
///
/// No credentials are involved, but the measurement belongs with the other
/// published timings.
#[tokio::test]
async fn live_snapshot_parse_stays_within_its_budget() {
    if live_project().is_none() {
        return;
    }

    let started = Instant::now();
    let snapshot = catalog::embedded_snapshot().expect("the embedded snapshot must parse");
    let parsed = snapshot
        .into_catalog()
        .expect("the committed snapshot must satisfy the namespacing invariants");
    let elapsed = started.elapsed();

    eprintln!(
        "snapshot parse: {elapsed:?} for {} services / {} tools",
        parsed.services.len(),
        parsed.tool_count()
    );
    assert!(
        elapsed < SNAPSHOT_PARSE_BUDGET,
        "snapshot parse took {elapsed:?}, over the {SNAPSHOT_PARSE_BUDGET:?} budget"
    );
}

/// One catalog refresh fan-out over the whole registry must finish in budget.
///
/// This measures the fan-out itself, not the delay before it is scheduled,
/// which is what ledger deviation #5a asks for.
#[tokio::test]
async fn live_background_catalog_refresh_completes_within_its_budget() {
    if live_project().is_none() {
        return;
    }

    let http = shared_http_client().expect("the shared HTTP client builds");
    let fallback = catalog::embedded_snapshot()
        .expect("the embedded snapshot must parse")
        .into_catalog()
        .expect("the committed snapshot must satisfy the namespacing invariants");

    let started = Instant::now();
    let fresh = catalog::Catalog::build_live(registry::ENDPOINTS, &http, Some(&fallback))
        .await
        .expect("a live fan-out degrades per host rather than failing outright");
    let elapsed = started.elapsed();

    let live_services = fresh
        .services
        .iter()
        .filter(|s| s.source == catalog::CatalogSource::Live)
        .count();
    eprintln!(
        "catalog refresh fan-out: {elapsed:?} for {} services ({live_services} live, {} tools)",
        fresh.services.len(),
        fresh.tool_count()
    );

    assert!(
        elapsed < REFRESH_BUDGET,
        "catalog refresh took {elapsed:?}, over the {REFRESH_BUDGET:?} budget"
    );
    assert!(
        live_services > 0,
        "no endpoint answered live; the measurement would be meaningless. \
         Reached {} services from the snapshot only",
        fresh.services.len()
    );
}
