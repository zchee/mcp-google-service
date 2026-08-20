//! Loading the committed snapshot into a [`Catalog`].
//!
//! This is the non-network part of `serve` startup. Since P5 the serve path
//! validates and materializes the embedded rkyv archive (`materialize_embedded`,
//! with `access_file` isolating the checked-validation cost); the serde JSON
//! rows remain because `--snapshot <PATH>` and `print-catalog` still parse
//! JSON, and because they are the before/after axis against `BASELINE.md`
//! section 2b (`parse_json` here is the same work `parse_embedded` measured
//! when the JSON was the embedded artifact).
//!
//! Allocations are counted by [`AllocProfiler`], which adds a thread-local
//! increment per allocation to the timed region.

use std::{path::Path, sync::LazyLock};

use divan::{AllocProfiler, Bencher, black_box};
use mcp_google_service::{
    archive,
    catalog::{self, Catalog, Snapshot},
    registry::{self, Endpoint},
    server,
};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

/// The committed snapshot, resolved against the crate root rather than the
/// working directory so `cargo bench` works from anywhere.
const SNAPSHOT_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/catalog-snapshot.json");

/// The committed archive the binary embeds, same resolution.
const ARCHIVE_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/catalog-snapshot.bin");

/// Samples per benchmark; each JSON sample is a ~10 ms parse, so 30 keeps the
/// whole file fast while leaving a usable distribution.
const SAMPLES: u32 = 30;

/// The snapshot JSON, read once; parsing is the measured work, not the read.
static SNAPSHOT_JSON: LazyLock<String> = LazyLock::new(|| {
    std::fs::read_to_string(SNAPSHOT_FILE).expect("the committed snapshot file reads")
});

/// The archive bytes, read once and leaked for the `'static` access API; the
/// allocator hands back 16-aligned blocks, which `archive::access` verifies
/// rather than trusts.
static ARCHIVE_BYTES: LazyLock<&'static [u8]> = LazyLock::new(|| {
    Box::leak(
        std::fs::read(ARCHIVE_FILE)
            .expect("the committed archive file reads")
            .into_boxed_slice(),
    )
});

fn main() {
    divan::main();
}

/// Snapshot JSON text -> [`Snapshot`]; the serde path `--snapshot` still pays.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn parse_json() -> Snapshot {
    serde_json::from_str(black_box(SNAPSHOT_JSON.as_str())).expect("the committed snapshot parses")
}

/// Read `data/catalog-snapshot.json` from disk and parse it.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn parse_file() -> Snapshot {
    catalog::load_snapshot_file(black_box(Path::new(SNAPSHOT_FILE)))
        .expect("the committed snapshot file loads")
}

/// Parse plus the namespacing validation: [`Snapshot`] -> [`Catalog`].
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn parse_json_into_catalog() -> Catalog {
    serde_json::from_str::<Snapshot>(black_box(SNAPSHOT_JSON.as_str()))
        .expect("the committed snapshot parses")
        .into_catalog()
        .expect("the committed snapshot satisfies the namespacing invariants")
}

/// Checked validation of the archive alone: header plus `rkyv::access`.
///
/// This is the price of refusing `access_unchecked`; the plan requires it to
/// stay a small fraction of what the parse used to cost.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn access_file(bencher: Bencher) {
    let bytes: &'static [u8] = &ARCHIVE_BYTES;
    bencher.bench(|| archive::access(black_box(bytes)).expect("the committed archive validates"));
}

/// What `serve` startup now does: validate the embedded archive and
/// materialize the catalog, schemas staying compressed.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn materialize_embedded() -> Catalog {
    catalog::embedded_catalog().expect("the embedded archive materializes")
}

/// The deferred half of the lazy design: `describe_tools`' first touch of a
/// tool inflates and parses its schema frames, once. A fresh catalog per
/// iteration keeps every touch a first touch.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn describe_first_touch_one_tool(bencher: Bencher) {
    bencher
        .with_inputs(|| catalog::embedded_catalog().expect("the embedded archive materializes"))
        .bench_refs(|materialized| {
            let entry = materialized
                .get(black_box("run__list_services"))
                .expect("present in the committed catalog");
            let input = entry.tool.input_schema().expect("archived schemas inflate");
            let output = entry
                .tool
                .output_schema()
                .expect("archived schemas inflate");
            (input.len(), output.map(|schema| schema.len()))
        });
}

/// What `serve` does with a materialized catalog: narrow to the exposed
/// endpoints, relabel as snapshot-sourced, freeze.
///
/// With no pruning in effect every registered endpoint is exposed, which is
/// the largest input this step can see.
#[divan::bench(sample_count = SAMPLES, sample_size = 1)]
fn assemble_serve_catalog_all_endpoints(bencher: Bencher) {
    let exposed: Vec<&Endpoint> = registry::ENDPOINTS.iter().collect();
    bencher
        .with_inputs(|| catalog::embedded_catalog().expect("the embedded archive materializes"))
        .bench_values(|materialized| {
            server::assemble_serve_catalog(materialized, black_box(&exposed))
                .expect("the embedded catalog satisfies the namespacing invariants")
        });
}
