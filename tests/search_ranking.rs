//! Golden-file test for the ranking `Catalog::search` produces over the
//! committed snapshot.
//!
//! `search_tools` is the first thing a model calls, and it reads the head of
//! the result list, so the *order* is part of the server's behaviour rather
//! than an implementation detail. The real-model E2E of 2026-08-20 found the
//! query `"cloud run"` ranking BigQuery-related tools above Cloud Run's own;
//! `tests/golden/search-ranking.txt` pins that case and a set of others so a
//! ranking change can never land silently again.
//!
//! Two layers: the golden file pins the exact head of each ranking, and the
//! property tests below state the reasons those heads are right, so a future
//! edit to the golden file can be judged against them.

use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::Path,
};

use mcp_google_service::catalog::{self, Catalog};

/// Sentinel a golden block uses to pin an empty result.
const NO_HITS: &str = "(no hits)";

/// Queries `benches/bench_search.rs` measures; each must be pinned here so
/// the latency numbers and the ranking contract describe the same queries.
const BENCHMARK_QUERIES: &[&str] =
    &["instances", "cloud run", "list cloud run", "zzzznomatch", "list cloud zzzznomatch"];

/// One `query:` block of the golden file.
#[derive(Debug)]
struct GoldenBlock {
    /// Line number of the `query:` line, for failure messages.
    line: usize,
    query: String,
    /// Expected head of the ranking, in order; empty pins an empty result.
    expected: Vec<String>,
}

/// The catalog the binary ships.
fn committed_catalog() -> Catalog {
    catalog::embedded_catalog().expect("the embedded archive materializes")
}

/// Namespaced names in rank order for `query` over the whole catalog.
fn ranking(catalog: &Catalog, query: &str) -> Vec<String> {
    catalog.search(query, None).into_iter().map(|tool| tool.namespaced_name.clone()).collect()
}

/// The score `query` gives `name`, or `None` when the tool is not a hit.
fn score_of(catalog: &Catalog, query: &str, name: &str) -> Option<u32> {
    let mut found = None;
    catalog.search_with(query, None, usize::MAX, |hit| {
        if hit.tool.namespaced_name == name {
            found = Some(hit.score);
        }
    });
    found
}

/// Every namespaced name a service exposes.
fn tools_of(catalog: &Catalog, service_id: &str) -> BTreeSet<String> {
    catalog
        .service(service_id)
        .unwrap_or_else(|| panic!("{service_id} is in the snapshot"))
        .tools
        .iter()
        .map(|t| t.namespaced_name.clone())
        .collect()
}

/// Parse `tests/golden/search-ranking.txt`.
fn golden_blocks() -> Vec<GoldenBlock> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/search-ranking.txt");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    let mut blocks: Vec<GoldenBlock> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = raw.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if let Some(query) = content.strip_prefix("query:") {
            blocks.push(GoldenBlock { line, query: query.trim().to_owned(), expected: Vec::new() });
            continue;
        }
        let block = blocks
            .last_mut()
            .unwrap_or_else(|| panic!("line {line}: `{content}` appears before any `query:` line"));
        if content == NO_HITS {
            assert!(
                block.expected.is_empty(),
                "line {line}: `{NO_HITS}` must be the only entry of its block"
            );
            continue;
        }
        assert!(content.contains("__"), "line {line}: `{content}` is not a namespaced tool name");
        block.expected.push(content.to_owned());
    }

    let mut seen = HashSet::new();
    for block in &blocks {
        assert!(
            seen.insert(block.query.as_str()),
            "line {}: query `{}` is pinned twice",
            block.line,
            block.query
        );
    }
    assert!(!blocks.is_empty(), "the golden file pins nothing");
    blocks
}

#[test]
fn golden_file_pins_the_head_of_every_ranking() {
    let catalog = committed_catalog();
    let mut failures = Vec::new();

    for block in golden_blocks() {
        let actual = ranking(&catalog, &block.query);
        if block.expected.is_empty() {
            if !actual.is_empty() {
                failures.push(format!(
                    "line {}: `{}` should have no hits, got {} (first: {})",
                    block.line,
                    block.query,
                    actual.len(),
                    actual.first().map_or("", String::as_str)
                ));
            }
            continue;
        }
        let head: Vec<&str> =
            actual.iter().take(block.expected.len()).map(String::as_str).collect();
        let expected: Vec<&str> = block.expected.iter().map(String::as_str).collect();
        if head != expected {
            let shown: Vec<&str> = actual.iter().take(10).map(String::as_str).collect();
            failures.push(format!(
                "line {}: `{}`\n    expected head: {expected:?}\n    actual head:   {head:?}\n    actual top 10: {shown:?}",
                block.line, block.query
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} golden block(s) diverge from the ranking:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn golden_file_covers_the_benchmark_queries() {
    let pinned: HashSet<String> = golden_blocks().into_iter().map(|b| b.query).collect();
    for query in BENCHMARK_QUERIES {
        assert!(
            pinned.contains(*query),
            "benchmark query `{query}` is not pinned in tests/golden/search-ranking.txt"
        );
    }
}

#[test]
fn golden_file_names_only_tools_that_exist() {
    let catalog = committed_catalog();
    for block in golden_blocks() {
        for name in &block.expected {
            assert!(
                catalog.get(name).is_some(),
                "line {}: `{name}` is not in the committed snapshot",
                block.line
            );
        }
    }
}

/// The E2E finding, stated as the property rather than the exact order: for
/// `"cloud run"` every one of Cloud Run's tools precedes every BigQuery tool
/// and the `bq` CLI tool the E2E saw ranked above them.
#[test]
fn cloud_run_ranks_every_cloud_run_tool_above_every_bigquery_tool() {
    let catalog = committed_catalog();
    let ranked = ranking(&catalog, "cloud run");

    let run_tools: BTreeSet<&str> = catalog
        .service("run")
        .expect("run is in the snapshot")
        .tools
        .iter()
        .map(|t| t.namespaced_name.as_str())
        .collect();
    assert!(!run_tools.is_empty());

    let head: BTreeSet<&str> = ranked.iter().take(run_tools.len()).map(String::as_str).collect();
    assert_eq!(
        head, run_tools,
        "the head of the `cloud run` ranking must be exactly Cloud Run's tools; got {ranked:?}"
    );

    let position = |name: &str| ranked.iter().position(|n| n == name);
    let last_run = run_tools.iter().filter_map(|n| position(n)).max().expect("run tools are hits");
    for offender in ["bigquery__execute_sql_readonly", "cloudcli__run_bq_command"] {
        let at = position(offender)
            .unwrap_or_else(|| panic!("`{offender}` matches `cloud run` (it mentions both words)"));
        assert!(at > last_run, "`{offender}` at {at} ranks above a Cloud Run tool at {last_run}");
    }
}

#[test]
fn a_query_that_spells_a_service_id_puts_that_service_first() {
    let catalog = committed_catalog();
    for (query, service_id) in [
        ("run", "run"),
        ("bigquery", "bigquery"),
        ("cloud sql", "sqladmin"),
        ("cloud asset", "cloudasset"),
        ("resource manager", "cloudresourcemanager"),
        ("bigquery data transfer", "bigquerydatatransfer"),
        ("error reporting", "clouderrorreporting"),
        ("vertex generate", "vertex-generate"),
        ("vtx notebook", "vtx-notebook"),
    ] {
        let ranked = ranking(&catalog, query);
        let own = tools_of(&catalog, service_id).len();
        let head: Vec<&str> = ranked.iter().take(own.min(5)).map(String::as_str).collect();
        assert!(
            !head.is_empty() && head.iter().all(|n| n.starts_with(&format!("{service_id}__"))),
            "`{query}` should lead with `{service_id}__*`; head is {head:?}"
        );
    }
}

/// The 2026-09-01 review finding: a `-` in a service id is a word boundary,
/// so `vertex generate` names `vertex-generate` and every tool of that suite
/// is a hit, exactly as `cloud asset` names `cloudasset`. Before the fix the
/// tokens were concatenated against the raw id, `vertexgenerate` never
/// matched `vertex-generate`, and the whole suite fell to a prefix credit
/// that a single unrelated description hit could outrank.
#[test]
fn a_hyphenated_service_id_is_spelled_word_by_word() {
    let catalog = committed_catalog();
    for (query, service_id) in
        [("vertex generate", "vertex-generate"), ("vtx notebook", "vtx-notebook")]
    {
        let own = tools_of(&catalog, service_id);
        assert!(!own.is_empty(), "{service_id} is in the snapshot");
        let ranked = ranking(&catalog, query);
        let head: BTreeSet<String> = ranked.iter().take(own.len()).cloned().collect();
        assert_eq!(
            head, own,
            "`{query}` must lead with every `{service_id}` tool; ranking was {ranked:?}"
        );
        // The credit is the whole-id one: a tool whose name says nothing about
        // the query still scores what `cloud asset` gives `cloudasset`'s tools.
        let literal = ranking(&catalog, service_id);
        assert_eq!(
            literal.iter().take(own.len()).cloned().collect::<BTreeSet<_>>(),
            own,
            "the id typed literally (`{service_id}`) names the same suite"
        );
    }

    // The single-word spelling that only *starts* the id is still a prefix
    // credit, so `vertex` alone reaches every `vertex-*` suite ...
    let vertex: BTreeSet<String> = catalog
        .tools()
        .filter(|t| t.service_id.starts_with("vertex-"))
        .map(|t| t.namespaced_name.clone())
        .collect();
    let hits: BTreeSet<String> = ranking(&catalog, "vertex").into_iter().collect();
    assert!(
        vertex.is_subset(&hits),
        "`vertex` should reach every `vertex-*` tool; missing {:?}",
        vertex.difference(&hits).collect::<Vec<_>>()
    );
    // ... but not the notebook suite, whose id had to drop the `vertex-`
    // prefix for the 64-char name limit. The README says it is reached
    // through `notebook` and `colab`: each leads with the suite's own tools,
    // every other hit trailing behind them.
    let suite = |name: &String| name.starts_with("vtx-notebook__");
    for query in ["notebook", "colab"] {
        let hits = ranking(&catalog, query);
        let last_own = hits.iter().rposition(suite);
        let first_other = hits.iter().position(|n| !suite(n));
        assert!(last_own.is_some(), "`{query}` reaches the notebook suite");
        assert!(
            first_other.is_none_or(|other| last_own.is_some_and(|own| own < other)),
            "`{query}` must lead with the notebook suite; ranking was {hits:?}"
        );
    }
}

/// The `list cloud run` golden block puts a Colab Enterprise tool above the
/// gcloud escape hatch. The mechanism, so the block can be judged on it: the
/// notebook tool has `list` as a name word, `run` as the start of its name
/// word `runtime`, and all three tokens in its description, while
/// `run_gcloud_command` has `run` as a name word but `cloud` only inside
/// `gcloud`. A name word and a word prefix outweigh a substring, so the
/// notebook tool scores higher. Neither tool's service is named by the
/// query: `cloud run` names `run`, whose `list_services` leads.
#[test]
fn list_cloud_run_credits_name_words_and_word_prefixes_over_substrings() {
    let catalog = committed_catalog();
    const QUERY: &str = "list cloud run";
    const NOTEBOOK: &str = "vtx-notebook__colab_enterprise_list_notebook_runtime_templates";
    const GCLOUD: &str = "cloudcli__run_gcloud_command";

    let ranked = ranking(&catalog, QUERY);
    assert_eq!(ranked.first().map(String::as_str), Some("run__list_services"));
    let position = |name: &str| {
        ranked
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("`{name}` matches `{QUERY}`"))
    };
    assert!(position(NOTEBOOK) < position(GCLOUD), "ranking was {ranked:?}");
    let score = |name: &str| score_of(&catalog, QUERY, name).expect("both are hits");
    assert!(score(NOTEBOOK) > score(GCLOUD), "{} vs {}", score(NOTEBOOK), score(GCLOUD));

    let tool =
        |name: &str| catalog.get(name).unwrap_or_else(|| panic!("`{name}` is in the snapshot"));
    let words = |name: &str| -> Vec<&str> { tool(name).upstream_name().split('_').collect() };
    let description = |name: &str| tool(name).tool.description().unwrap_or("").to_lowercase();

    let notebook_words = words(NOTEBOOK);
    assert!(notebook_words.contains(&"list"), "`list` is a name word: {notebook_words:?}");
    assert!(
        notebook_words.iter().any(|w| w.starts_with("run") && *w != "run"),
        "`run` only starts a name word (`runtime`): {notebook_words:?}"
    );
    assert!(
        !notebook_words.iter().any(|w| w.contains("cloud")),
        "`cloud` is not in the name, so it can only be a description hit: {notebook_words:?}"
    );
    let notebook_description = description(NOTEBOOK);
    for token in ["list", "cloud", "run"] {
        assert!(notebook_description.contains(token), "`{token}` is in the description");
    }

    let gcloud_words = words(GCLOUD);
    assert!(gcloud_words.contains(&"run"), "`run` is a name word: {gcloud_words:?}");
    assert!(
        !gcloud_words.contains(&"cloud") && gcloud_words.iter().any(|w| w.contains("cloud")),
        "`cloud` is only a substring of `gcloud`: {gcloud_words:?}"
    );
}

#[test]
fn every_execute_sql_tool_leads_the_execute_sql_ranking() {
    let catalog = committed_catalog();
    let execute_sql: BTreeSet<&str> = catalog
        .tools()
        .filter(|t| t.upstream_name().starts_with("execute_sql"))
        .map(|t| t.namespaced_name.as_str())
        .collect();
    assert!(execute_sql.len() >= 4, "sanity: several services expose execute_sql");

    let ranked = ranking(&catalog, "execute sql");
    let head: BTreeSet<&str> = ranked.iter().take(execute_sql.len()).map(String::as_str).collect();
    assert_eq!(head, execute_sql, "ranking was {ranked:?}");
}

#[test]
fn search_is_conjunctive_and_a_miss_is_empty() {
    let catalog = committed_catalog();
    assert!(ranking(&catalog, "zzzznomatch").is_empty());
    assert!(ranking(&catalog, "list cloud zzzznomatch").is_empty());
    assert!(
        ranking(&catalog, "list cloud run").len() < ranking(&catalog, "cloud run").len(),
        "an extra token narrows, never widens"
    );
}

#[test]
fn search_is_case_insensitive_and_whitespace_tolerant() {
    let catalog = committed_catalog();
    let canonical = ranking(&catalog, "cloud run");
    assert_eq!(ranking(&catalog, "  Cloud   RUN "), canonical);
    assert_eq!(ranking(&catalog, "CLOUD\tRun"), canonical);
}

#[test]
fn the_service_filter_restricts_without_changing_relative_order() {
    let catalog = committed_catalog();
    let filtered = catalog.search("list", Some("run"));
    assert!(!filtered.is_empty());
    assert!(filtered.iter().all(|t| t.service_id == "run"));
    assert_eq!(filtered[0].namespaced_name, "run__list_services");

    let unfiltered = ranking(&catalog, "list");
    let run_only: Vec<&str> =
        unfiltered.iter().filter(|n| n.starts_with("run__")).map(String::as_str).collect();
    let filtered_names: Vec<&str> = filtered.iter().map(|t| t.namespaced_name.as_str()).collect();
    assert_eq!(filtered_names, run_only);
}

#[test]
fn an_empty_query_lists_everything_in_name_order() {
    let catalog = committed_catalog();
    let all = ranking(&catalog, "");
    assert_eq!(all.len(), catalog.tool_count());
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted, "an empty query must be in namespaced-name order");
}
