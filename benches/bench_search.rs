//! `Catalog::search` over the committed 47-service / 548-tool snapshot.
//!
//! Two temperatures per query:
//!
//! * `cold` -- the first search against a freshly materialized [`Catalog`]
//!   (a deep copy built per iteration, outside the timed region). Nothing is
//!   precomputed on that copy, so any index a later implementation builds
//!   lazily is built inside the timed region. This is not a CPU-cache flush;
//!   it is "first query on a new catalog".
//! * `warm` -- repeated searches against one long-lived catalog, which is what
//!   every `search_tools` call after the first sees in the running server.
//!
//! The query set covers 1-token and 3-token queries, hits and misses, plus the
//! 2-token `"cloud run"` query the real-model E2E flagged for ranking. Matching
//! is conjunctive, so the 3-token miss exercises the early exit after two
//! matching tokens.
//!
//! Allocations are counted by [`AllocProfiler`]: the optimization plan's
//! acceptance criterion for the search rewrite is zero allocations on the
//! query path, so the counter has to exist before the rewrite. Counting adds a
//! thread-local increment per allocation to the timed region.

use std::sync::LazyLock;

use divan::{AllocProfiler, Bencher, black_box, counter::ItemsCount};
use mcp_google_service::catalog::{self, Catalog};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

/// The catalog the binary ships, parsed once per process.
static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    catalog::embedded_snapshot()
        .expect("embedded snapshot parses")
        .into_catalog()
        .expect("embedded snapshot satisfies the namespacing invariants")
});

/// Queries, chosen to hit distinct paths through the scorer.
const QUERIES: &[&str] = &[
    // 1 token, hit: matches many names (`*_instances`) and descriptions.
    "instances",
    // 2 tokens, hit: the real-model E2E query whose ranking P4 must fix.
    "cloud run",
    // 3 tokens, hit: every token must match name or description.
    "list cloud run",
    // 1 token, miss: scans every tool and matches nothing.
    "zzzznomatch",
    // 3 tokens, miss: the first two match broadly, the third never does.
    "list cloud zzzznomatch",
];

fn main() {
    divan::main();
}

#[divan::bench(args = QUERIES, sample_count = 20, sample_size = 1)]
fn cold(bencher: Bencher, query: &str) {
    bencher
        .with_inputs(|| CATALOG.clone())
        .counter(ItemsCount::new(CATALOG.tool_count()))
        // The hit list borrows the per-iteration catalog, so it cannot be
        // handed back to divan for deferred dropping the way `warm` does; it
        // is counted and dropped inside the timed region instead.
        .bench_refs(|catalog| black_box(catalog.search(black_box(query), None).len()));
}

#[divan::bench(args = QUERIES)]
fn warm(bencher: Bencher, query: &str) {
    let catalog = &*CATALOG;
    bencher
        .counter(ItemsCount::new(catalog.tool_count()))
        .bench(|| catalog.search(black_box(query), None));
}

/// The service-filtered path: only one service's tools are scored.
#[divan::bench]
fn warm_filtered_to_run(bencher: Bencher) {
    let catalog = &*CATALOG;
    let scanned = catalog.service("run").map_or(0, |s| s.tools.len());
    bencher
        .counter(ItemsCount::new(scanned))
        .bench(|| catalog.search(black_box("list"), Some(black_box("run"))));
}

/// An empty query returns everything; it is the cheapest possible scan.
#[divan::bench]
fn warm_empty_query(bencher: Bencher) {
    let catalog = &*CATALOG;
    bencher
        .counter(ItemsCount::new(catalog.tool_count()))
        .bench(|| catalog.search(black_box(""), None));
}
