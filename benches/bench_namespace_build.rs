//! Namespacing the 548 upstream tools and validating the catalog invariants.
//!
//! Two steps, measured separately because they run at different times:
//!
//! * `namespace_tools` -- `NamespacedTool::new` over every upstream tool, the
//!   `{service}__{tool}` naming the live fan-out performs per fetched tool.
//! * `catalog_new` -- `Catalog::new` over the 47 per-service catalogs: sort
//!   services and tools, then enforce global uniqueness and the 64-char name
//!   limit. The snapshot stores its services already sorted, so this is the
//!   input shape `Snapshot::into_catalog` sees.
//!
//! Inputs are cloned outside the timed region. Allocations are counted by
//! [`AllocProfiler`], which adds a thread-local increment per allocation to
//! the timed region.

use std::sync::LazyLock;

use divan::{AllocProfiler, Bencher, black_box, counter::ItemsCount};
use mcp_google_service::catalog::{self, Catalog, NamespacedTool, ServiceCatalog};
use rmcp::model::Tool;

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

/// The catalog the binary ships, parsed once per process.
static CATALOG: LazyLock<Catalog> =
    LazyLock::new(|| catalog::embedded_catalog().expect("the embedded archive materializes"));

fn main() {
    divan::main();
}

/// `(service_id, upstream tool)` for every tool, as the fan-out sees them.
fn upstream_tools() -> Vec<(&'static str, Tool)> {
    CATALOG
        .services
        .iter()
        .flat_map(|service| {
            service.tools.iter().map(move |entry| {
                let tool = entry.tool.to_rmcp().expect("archived schemas inflate");
                (service.service_id.as_str(), tool)
            })
        })
        .collect()
}

#[divan::bench(sample_count = 100, sample_size = 1)]
fn namespace_tools(bencher: Bencher) {
    bencher
        .with_inputs(upstream_tools)
        .counter(ItemsCount::new(CATALOG.tool_count()))
        .bench_values(|tools| {
            tools
                .into_iter()
                .map(|(service_id, tool)| NamespacedTool::new(black_box(service_id), tool))
                .collect::<Vec<NamespacedTool>>()
        });
}

#[divan::bench(sample_count = 100, sample_size = 1)]
fn catalog_new(bencher: Bencher) {
    bencher
        .with_inputs(|| CATALOG.services.clone())
        .counter(ItemsCount::new(CATALOG.tool_count()))
        .bench_values(|services: Vec<ServiceCatalog>| {
            Catalog::new(black_box(services)).expect("the committed catalog is valid")
        });
}
