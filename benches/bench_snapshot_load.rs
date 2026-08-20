//! Loading the committed snapshot into a [`Catalog`].
//!
//! This is the non-network part of `serve` startup: parse the snapshot JSON,
//! validate the namespacing invariants, then narrow and relabel it for the
//! exposed endpoints. The embedded copy and the on-disk copy are the same
//! bytes; the file variant adds the read so the `--snapshot <PATH>` path and
//! `print-catalog` are covered too.
//!
//! Allocations are counted by [`AllocProfiler`], which adds a thread-local
//! increment per allocation to the timed region.

use std::path::Path;

use divan::{AllocProfiler, Bencher, black_box};
use mcp_google_service::{
    catalog::{self, Catalog, Snapshot},
    registry::{self, Endpoint},
    server,
};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

/// The committed snapshot, resolved against the crate root rather than the
/// working directory so `cargo bench` works from anywhere.
const SNAPSHOT_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/catalog-snapshot.json");

/// Samples per benchmark; each sample is one ~45 ms parse, so 30 keeps the
/// whole file under a minute while leaving a usable distribution.
const SAMPLES: u32 = 30;

fn main() {
    divan::main();
}

/// JSON text compiled into the binary -> [`Snapshot`].
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn parse_embedded() -> Snapshot {
    catalog::embedded_snapshot().expect("embedded snapshot parses")
}

/// Read `data/catalog-snapshot.json` from disk and parse it.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn parse_file() -> Snapshot {
    catalog::load_snapshot_file(black_box(Path::new(SNAPSHOT_FILE)))
        .expect("the committed snapshot file loads")
}

/// Parse plus the namespacing validation: [`Snapshot`] -> [`Catalog`].
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn parse_embedded_into_catalog() -> Catalog {
    catalog::embedded_snapshot()
        .expect("embedded snapshot parses")
        .into_catalog()
        .expect("embedded snapshot satisfies the namespacing invariants")
}

/// What `serve` does with an already-parsed snapshot: validate, narrow to the
/// exposed endpoints, relabel as snapshot-sourced, freeze.
///
/// With no pruning in effect every registered endpoint is exposed, which is
/// the largest input this step can see.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn assemble_serve_catalog_all_endpoints(bencher: Bencher) {
    let exposed: Vec<&Endpoint> = registry::ENDPOINTS.iter().collect();
    bencher
        .with_inputs(|| catalog::embedded_snapshot().expect("embedded snapshot parses"))
        .bench_values(|snapshot| {
            server::assemble_serve_catalog(snapshot, black_box(&exposed))
                .expect("embedded snapshot satisfies the namespacing invariants")
        });
}
