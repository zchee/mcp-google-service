//! `Catalog` search over the committed 47-service / 548-tool snapshot.
//!
//! Two temperatures per query:
//!
//! * `cold` -- the first search against a freshly materialized [`Catalog`]
//!   (a deep copy built per iteration, outside the timed region; a clone
//!   carries no index, so the timed region includes building it). This is
//!   "first query after a catalog swap" in the running server.
//! * `warm` -- repeated searches against one long-lived catalog, which is
//!   what every `search_tools` call after the first sees.
//!
//! The query set covers 1-token and 3-token queries, hits and misses, plus
//! the 2-token `"cloud run"` query the real-model E2E flagged for ranking;
//! `tests/golden/search-ranking.txt` pins the orderings these queries return.
//! Matching is conjunctive, so the 3-token miss exercises the early exit.
//!
//! Allocations are counted by [`AllocProfiler`]: the optimization plan's
//! acceptance criterion for P4 is zero allocations on the query path, so
//! `warm` measures [`Catalog::search_with`] with the serve path's default
//! limit -- the call `search_tools` makes. `warm_collected` keeps measuring
//! the allocating [`Catalog::search`] wrapper, which is the same function the
//! `BASELINE.md` section 2a numbers were taken from (1,099 allocations and
//! 275.7 KB per query at `d341bbb`).

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

/// What `search_tools` passes when the caller does not name a limit; mirrors
/// `DEFAULT_SEARCH_LIMIT` in `src/server.rs`, which is private on purpose.
const SERVE_LIMIT: usize = 20;

/// Queries, chosen to hit distinct paths through the scorer.
const QUERIES: &[&str] = &[
    // 1 token, hit: matches many names (`*_instances`) and descriptions.
    "instances",
    // 2 tokens, hit: the real-model E2E query whose ranking P4 fixed.
    "cloud run",
    // 3 tokens, hit: every token must match the tool or its service.
    "list cloud run",
    // 1 token, miss: scans every tool and matches nothing.
    "zzzznomatch",
    // 3 tokens, miss: the first two match broadly, the third never does.
    "list cloud zzzznomatch",
];

fn main() {
    divan::main();
}

/// Fold the delivered hits into a value divan cannot optimize away.
fn drain(catalog: &Catalog, query: &str, service: Option<&str>, limit: usize) -> (usize, u64) {
    let mut folded = 0u64;
    let total = catalog.search_with(black_box(query), black_box(service), limit, |hit| {
        folded = folded
            .wrapping_add(u64::from(hit.score))
            .wrapping_add(hit.tool.namespaced_name.len() as u64);
    });
    (total, folded)
}

#[divan::bench(args = QUERIES, sample_count = 20, sample_size = 1)]
fn cold(bencher: Bencher, query: &str) {
    bencher
        .with_inputs(|| CATALOG.clone())
        .counter(ItemsCount::new(CATALOG.tool_count()))
        .bench_refs(|catalog| black_box(drain(catalog, query, None, SERVE_LIMIT)));
}

/// The serve path: ranked hits, default limit, no allocation.
#[divan::bench(args = QUERIES)]
fn warm(bencher: Bencher, query: &str) {
    let catalog = &*CATALOG;
    catalog.search_with("", None, 1, |_| {}); // build the index outside the timing
    bencher
        .counter(ItemsCount::new(catalog.tool_count()))
        .bench(|| black_box(drain(catalog, black_box(query), None, SERVE_LIMIT)));
}

/// The whole ranking delivered, still without allocating.
#[divan::bench(args = QUERIES)]
fn warm_unbounded(bencher: Bencher, query: &str) {
    let catalog = &*CATALOG;
    catalog.search_with("", None, 1, |_| {});
    bencher
        .counter(ItemsCount::new(catalog.tool_count()))
        .bench(|| black_box(drain(catalog, black_box(query), None, usize::MAX)));
}

/// The allocating wrapper `BASELINE.md` section 2a measured, for the delta.
#[divan::bench(args = QUERIES)]
fn warm_collected(bencher: Bencher, query: &str) {
    let catalog = &*CATALOG;
    catalog.search_with("", None, 1, |_| {});
    bencher
        .counter(ItemsCount::new(catalog.tool_count()))
        .bench(|| catalog.search(black_box(query), None));
}

/// The service-filtered path: only one service's tools are scored.
#[divan::bench]
fn warm_filtered_to_run(bencher: Bencher) {
    let catalog = &*CATALOG;
    catalog.search_with("", None, 1, |_| {});
    let scanned = catalog.service("run").map_or(0, |s| s.tools.len());
    bencher
        .counter(ItemsCount::new(scanned))
        .bench(|| black_box(drain(catalog, black_box("list"), Some("run"), SERVE_LIMIT)));
}

/// An empty query matches everything; ranking it is the selection floor.
#[divan::bench]
fn warm_empty_query(bencher: Bencher) {
    let catalog = &*CATALOG;
    catalog.search_with("", None, 1, |_| {});
    bencher
        .counter(ItemsCount::new(catalog.tool_count()))
        .bench(|| black_box(drain(catalog, black_box(""), None, SERVE_LIMIT)));
}
