# BASELINE

Fixed reference for the optimization plan
(`.omc/plans/2026-08-20-optimization-to-the-limit.md`). Every later phase reports
its delta against the numbers here, taken with the same harness, the same flag
regime, and the same machine. Nothing in this file is copied from the plan's
section 0; every number was re-measured on 2026-08-20 and the deviations from
section 0 are called out explicitly at the end.

## Provenance

| | |
|---|---|
| Source | `ae92467` plus the P1 harness (this commit); no hot path touched |
| Machine | Apple M3 Max, 16 cores, 128 GiB; Darwin 27.0.0 arm64 |
| Toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo 1.97.1` (pinned by `rust-toolchain.toml`) |
| Release binary | `target/release/mcp-google-service`, 30,442,048 bytes, sha256 `068f52bd0c8446a5ef895c7a8917c2922b5276ac159d28cd8b146c32155ad231`, built 2026-08-20T14:59:46+0900 |
| Flag regime | **`RUSTFLAGS` unset** for every build that produces a measured artifact; `[profile.release]` = `strip = "none"` only, so opt-level 3, codegen-units 16, no LTO, `panic = "unwind"` |
| Snapshot | `data/catalog-snapshot.json`, 11,637,929 bytes, 47 services / 548 tools, `generated_at` 2026-08-19T09:02:07Z |
| Quota project (real mode) | `gaudiy-licentia-dev` (the live tier's project), 56 APIs enabled, 14 of 47 services exposed after pruning |

### Why `RUSTFLAGS` is unset, and why that is the rule

The shell this repository is developed in exports nightly-only `RUSTFLAGS`
(`-Z ...`, `-C panic=abort`) that do not build on the pinned stable toolchain,
and the project `.envrc` (`layout rust_stable`) replaces them with
`-C target-cpu=native -C opt-level=3 -C codegen-units=1 -C force-frame-pointers=on
-C debug-assertions=off -C overflow-checks=off -C llvm-args=-unroll-threshold=500
-C llvm-args=-enable-dfa-jump-thread -C link-arg=-Wl,-dead_strip`. Builds,
clippy, tests and `cargo install` run under that set (`direnv exec . cargo ...`).
Benchmarks do not: a number that silently depends on an ambient environment
variable is not reproducible, and `target-cpu=native` makes it not even portable
between machines. Anything wanted in the shipped binary belongs in
`[profile.release]`, where it is recorded in the repository and where P7 must
justify each setting with a measured delta. `scripts/bench-startup.sh` refuses to
run with `RUSTFLAGS` set and prints the binary's size, sha256, mtime and the
toolchain with every result.

**`direnv exec .` is not a neutral wrapper for gates, and the rule above is
narrower than it looks.** Two effects, both measured:

* **Release builds are contaminated.** The `Reproduce` block below already
  builds with `env -u RUSTFLAGS` into the project `target/`, which was
  correct before this was understood; it is now correct *for a stated
  reason*. A release artifact built under `direnv` is not the shipped
  artifact.
* **`-C debug-assertions=off` deletes a test.** It makes `cfg(debug_assertions)`
  false, which removes both the `debug_assert!` in `Proxy::new` (routes must
  never leave `googleapis.com`) **and** the `#[should_panic]` test at
  `src/proxy.rs` that guards it. The pairing is deliberate and correct -- the
  test exists exactly when the assert does, so this is not a vacuous pass --
  but the consequence at team level is that a suite run under `direnv exec .`
  checks **one invariant fewer** than the same command with `RUSTFLAGS` unset,
  while both report a confident green. Gate runs use `env -u RUSTFLAGS`.

**Gate protocol, amended.** The blindness above was hit independently by the
verification lane on the same tree, and located by diffing the test roster:
under `direnv` the suite reports **189/189, no skip and no signal**, because
`a_route_off_google_trips_the_guard` is compiled out rather than skipped; under
`env -u RUSTFLAGS` it is **190/190** with that test passing. A vanished test
leaves no trace in a pass count -- which is the same defect class the commit
that added it had just closed for `#[ignore]`, reopened through `cfg`. So:

| Gate | Environment |
|---|---|
| `fmt`, `clippy`, `doc`, `build` | `direnv exec . cargo ...` |
| **`nextest`** | **`env -u RUSTFLAGS cargo --config <dev> nextest run`** -- canonical count **190** |
| benches, live tier, startup probes | `env -u RUSTFLAGS`, serial |

`nextest` is the only gate whose *result set* -- not just its timings --
depends on ambient codegen flags. Running it with `RUSTFLAGS` unset makes the
count reproducible, matches this ledger's rule for anything compared against a
recorded number, and executes the suite with debug-assertions and
overflow-checks **on**, which is strictly stronger. The composite rule:
**`direnv` for `fmt`/`clippy`/`doc`/`build`; `env -u RUSTFLAGS` for `nextest`
and for anything timed.**

### Reproduce

```sh
# 1. the artifact under test (no RUSTFLAGS; project target/, not the dev tmpfs)
env -u RUSTFLAGS cargo build --release

# 2. startup probes, strictly serial, nothing else benchmarking meanwhile
env -u RUSTFLAGS GOOGLE_MCP_QUOTA_PROJECT=<project> scripts/bench-startup.sh --runs 20
env -u RUSTFLAGS scripts/bench-startup.sh --offline --runs 20
env -u RUSTFLAGS scripts/bench-startup.sh --print-catalog --runs 20

# 3. micro-benchmarks (divan), serial
env -u RUSTFLAGS cargo bench
```

`cargo bench` re-links `target/release/mcp-google-service` from the bench
profile (a 30,538,960-byte artifact with a different sha256). Run step 1 again
before any further startup probe; the sha256 the script prints is how a mixed-up
binary is caught.

## 1. Startup: process start to `tools/list` answered

`scripts/bench-startup.sh`, 20 serial runs each, 1 s pause between runs
(excluded), one client doing `initialize` -> `notifications/initialized` ->
`tools/list` over stdio. Time is from just before the spawn to the `tools/list`
response line being read. Two-tier surface, so the response is the four
meta-tools.

| Mode | What is stubbed | min | **median** | p95 | max | mean |
|---|---|---:|---:|---:|---:|---:|
| real | nothing: real ADC (`authorized_user` refresh token), real Service Usage listing, real 47-host background fan-out | 1625.25 | **1809.31** | 2406.72 | 2521.87 | 1915.92 |
| `--offline` | `GOOGLE_APPLICATION_CREDENTIALS` -> throwaway service-account key whose `token_uri` is a loopback stub; `HTTPS_PROXY`/`ALL_PROXY` -> `http://127.0.0.1:1`, so the Service Usage call and every fan-out connection fail with ECONNREFUSED in microseconds; no byte leaves the machine | 132.81 | **137.80** | 219.51 | 338.18 | 153.14 |
| `--print-catalog` | no MCP session at all: `print-catalog` process start -> exit, reading the snapshot from disk (cwd = repo root) and rendering the table | 24.62 | **26.12** | 28.45 | 29.24 | 26.40 |

All times in ms. Per-run values, sorted:

- real: 1625.25 1726.02 1740.09 1742.38 1750.40 1758.00 1795.08 1796.89 1805.30
  1805.50 1813.12 1872.04 1914.88 1960.36 1989.59 1994.99 2079.32 2220.53
  2406.72 2521.87
- offline: 132.81 133.08 133.64 133.72 134.11 134.21 134.41 134.75 135.56 136.72
  138.87 140.03 142.21 144.10 144.67 145.62 148.91 157.66 219.51 338.18
- print-catalog: 24.62 24.64 25.07 25.12 25.16 25.59 25.89 26.08 26.09 26.09
  26.16 26.25 26.80 26.98 27.24 27.26 27.54 27.79 28.45 29.24

Cross-check of `print-catalog` with hyperfine (`hyperfine -N --warmup 3 --runs 20`):
25.6 ms +- 1.1 ms for this binary; 24.7 ms +- 0.8 ms for the direnv-built
binary (`codegen-units=1`, `-dead_strip`), i.e. the aggressive flags buy 4% here.

Why `--offline` is built the way it is: gcp_auth retries a *refused* token
endpoint five times with 50/100/200/400 ms back-off, so pointing `token_uri` at a
dead port would add ~750 ms of sleep and call it "offline". The loopback stub
answers in well under a millisecond. reqwest reads `HTTPS_PROXY` from the
environment even with `default-features = false` (hyper-util's proxy matcher
always consults the env), so the dead proxy port stops every `https://` request
the shared client makes before a DNS lookup happens; gcp_auth's own hyper client
has no proxy support, which is why the stub is still reached.

### 1a. Where the time goes (one debug-logged run each, `RUST_LOG=debug`)

Single diagnostic runs, so these are attributions, not statistics. Spans are
between consecutive log lines of the binary and its dependencies.

Real mode, 1900.4 ms total:

| Span | ms | What it is |
|---|---:|---|
| exec -> first log line | ~8.5 | exec, dyld of a 30 MB binary, tokio runtime, tracing init |
| `gcp_auth::provider()`: `HttpClient::new()` | **121.5** | gcp_auth builds its own hyper client with the native root store (`rustls-native-certs` reads the macOS keychain). CPU/IO on the critical path, paid by every credential type, no network |
| ADC refresh-token exchange (`accounts.google.com`) | **199.0** | network: 1 RTT + TLS; `ConfigDefaultCredentials::with_client` fetches eagerly during discovery |
| Service Usage `services?filter=state:ENABLED` | **1516.0** | network: the single largest item, ~80% of startup |
| snapshot parse + `assemble_serve_catalog` + logging | 50.8 | 13.3 ms in the offline run below; the extra here is unexplained single-sample variance |
| `initialize` -> `tools/list` answered | ~1.4 | the handshake itself |

Offline mode, 139.2 ms total:

| Span | ms | What it is |
|---|---:|---|
| exec -> first log line | ~10 | as above |
| `gcp_auth::provider()` construction + JWT sign + loopback token exchange | **113.5** | `HttpClient::new()` (native roots) dominates; the token exchange against the stub is ~0.6 ms of it |
| Service Usage call refused via proxy | 0.2 | |
| snapshot parse + `assemble_serve_catalog` + logging | **13.3** | consistent with the in-process 9.9 ms parse + 0.3 ms assemble below |
| spawn refresh, bind stdio, `initialize` -> `tools/list` | ~2 | the 47 fan-out connections fail instantly on worker threads |

Consequence for P2: taking auth and pruning off the critical path removes the
two network spans **and** the 113-121 ms of gcp_auth construction. What is left
is ~25 ms (exec + parse + handshake), which is exactly what `print-catalog`
measures end to end. The plan's "<60 ms" target is therefore reachable by P2
alone; P5 (zero-copy snapshot) is what takes it from ~25 ms toward ~15 ms.

## 2. Micro-benchmarks (divan, `env -u RUSTFLAGS cargo bench`)

Medians; allocations are per iteration as counted by divan's `AllocProfiler`
(which adds a thread-local increment per allocation to the timed region).
Timer precision 41 ns.

### 2a. `bench_search` -- `Catalog::search` over 548 tools

| Query | tokens | hit/miss | cold median | warm median | allocs / iter | bytes / iter |
|---|---:|---|---:|---:|---:|---:|
| `instances` | 1 | hit | 61.99 µs | **47.47 µs** | 1,099 | 275.7 KB |
| `cloud run` | 2 | hit (the real-model E2E query) | 70.47 µs | **58.89 µs** | 1,099 | 275.7 KB |
| `list cloud run` | 3 | hit | 70.14 µs | **57.56 µs** | 1,099 | 275.7 KB |
| `zzzznomatch` | 1 | miss | 53.19 µs | **48.52 µs** | 1,098 | 275.6 KB |
| `list cloud zzzznomatch` | 3 | miss on 3rd | 65.92 µs | **59.11 µs** | 1,098 | 275.7 KB |
| `list` filtered to `run` | 1 | hit, 6 tools scanned | -- | 407.9 ns | 13 | 2.4 KB |
| `""` (empty) | 0 | returns all 548 | -- | 3.94 µs | 2 | 8.8 KB |

cold = first search on a freshly materialized `Catalog` (20 samples of 1);
warm = repeated searches on a long-lived catalog (100 samples). The 1,098-1,099
allocations per query are the 548 `name.to_lowercase()` + 548
`description.to_lowercase()` in `score_tool` (`src/catalog.rs:546-578`, the
`to_lowercase` calls at 551 and 556) plus the query, the token vector, the
scored vector and the result vector; 275.7 KB per query is those lowercased
copies (266,352 bytes of descriptions + 9,265 bytes of names, re-lowercased
on every query).

Note for P4: the plan expected search at "hundreds of µs". It is already
47-59 µs. P4's measurable wins are the allocation count (1,099 -> 0), the
ranking defect (`"cloud run"` must put `run__*` above BigQuery), and whatever
sub-50 µs is left; it is not a 10-100x latency win.

### 2b. `bench_snapshot_load`

| Benchmark | median | allocs / iter | bytes allocated / iter |
|---|---:|---:|---:|
| `parse_embedded` (embedded JSON -> `Snapshot`) | **9.86 ms** | 260,841 | 33.06 MB |
| `parse_embedded_into_catalog` (+ `Catalog::new`) | 9.73 ms | 260,858 | 33.21 MB |
| `parse_file` (read 11.6 MB from disk + parse) | 11.45 ms | 260,842 | 44.69 MB |
| `assemble_serve_catalog_all_endpoints` (validate + `restricted_to` + `marked_as`, snapshot pre-parsed, 47 endpoints) | **270.6 µs** | 4,809 | 1.00 MB |

30 samples of 1 each. Snapshot parse is ~10 ms in-process, ~33 MB of
allocations in 261k calls, and the dominant cost of the non-network path after
gcp_auth construction.

### 2c. `bench_namespace_build`

| Benchmark | median | allocs / iter |
|---|---:|---:|
| `namespace_tools` (548 x `NamespacedTool::new`) | 53.5 µs | 1,097 |
| `catalog_new` (sort 47 services + 548 tools, uniqueness + 64-char check) | 38.0 µs | 17 |

### 2d. `bench_classify_upstream`

| Case | median | allocs |
|---|---:|---:|
| `QuotaProjectMissing` (403) | 13.9 ns | 0 |
| `PermissionDenied` (403, generic) | 25.1 ns | 0 |
| `Internal500` (500, sanitize path) | 89.6 ns | 1 |
| `MissingCredential` (401) | 288.7 ns | 0 |
| `ServiceDisabled` (403, parses api + project) | 340.4 ns | 2 |
| `ServiceDisabledEnvelope` (403, JSON envelope) | 344.3 ns | 2 |
| `ApiKeyUnsupported` (401, second substring scan) | 489.3 ns | 0 |

Sub-microsecond throughout; recorded for completeness, not as a target.

## 3. Binary

`size -m target/release/mcp-google-service` (the measured artifact):

| Section | bytes | share |
|---|---:|---:|
| total file | 30,442,048 | 100% |
| `__TEXT,__const` (embedded snapshot + other constants) | 12,588,600 | 41.4% |
| `__TEXT,__text` (code) | 9,214,588 | 30.3% |
| `__LINKEDIT` | 5,816,320 | 19.1% |
| `__TEXT,__eh_frame` | 1,271,632 | 4.2% |
| `__TEXT,__gcc_except_tab` | 595,544 | 2.0% |
| `__DATA_CONST,__const` | 604,840 | 2.0% |
| `__TEXT,__unwind_info` | 236,880 | 0.8% |
| `__TEXT,__cstring` | 145,859 | 0.5% |

For comparison, the direnv-built binary (`codegen-units=1`, `-dead_strip`,
installed at `/opt/local/rust/cargo/bin`, sha256 `4d5e6e6d...`) is 25,260,448
bytes: `__text` 7,280,712, `__LINKEDIT` 3,358,720, `__const` 12,538,184. Those
flags remove ~5.2 MB (17%) of code and link-edit data and change the embedded
payload by 50 KB; they do not change startup (see section 5).

## 4. Snapshot facts (static, from `data/catalog-snapshot.json`)

| Fact | value | how |
|---|---:|---|
| services / tools | 47 / 548 | `jq` |
| description bytes, all tools (UTF-8) | 266,352 | `jq utf8bytelength` over `.tool.description` |
| tool-name bytes | 9,265 | same, over `.tool.name` |
| longest description | 39,874 bytes | |
| `inputSchema` bytes, compact JSON | 1,801,451 | `tojson \| utf8bytelength` |
| `outputSchema` bytes, compact JSON | 5,149,694 | same |
| schemas total, compact | 6,951,145 (59.7% of the file) | |
| file on disk (pretty-printed) | 11,637,929 | |

## 5. Section 0 of the plan, checked

Confirmed as stated:

- `src/catalog.rs:546-578` `score_tool` lowercases the tool name and the whole
  description for every tool on every query (`to_lowercase` at 551 and 556);
  the bench shows it as 1,099 allocations / 275.7 KB per query.
- `src/proxy.rs:126-141` builds a transport, runs a full MCP `initialize`
  handshake (`().serve(transport)`, line 132-135), issues the one `call_tool`,
  then cancels the session -- per dispatch.
- Startup total: section 0 quoted 1757 / 1924 / 1961 ms from 3 runs; measured
  median here is **1809 ms** over 20 runs (-6%), inside the 10% band. The p95 of
  2407 ms is not comparable to a 3-run figure.
- Description bytes: 266,352 measured vs 266,348 quoted (4 bytes, method).
- Two network round trips on the startup critical path: confirmed and now
  attributed (section 1a): ADC ~0.2 s, Service Usage ~1.5 s.

**Differs from section 0 by more than 10% -- use the numbers here:**

| Item | section 0 | measured here | delta | why |
|---|---:|---:|---:|---|
| release binary | 25,311,328 B | **30,442,048 B** | +20% | section 0 measured a binary built under the direnv flag set (`codegen-units=1`, `-Wl,-dead_strip`; its `__const` 12,538,184 matches that build exactly); the canonical artifact is built from `[profile.release]` alone |
| `__text` | 7,283,144 B | **9,214,588 B** | +27% | same cause |
| snapshot parse | 45 ms | **9.9 ms** in-process (`parse_embedded`), 13.3 ms for parse + assemble + log inside the serving process | -78% | section 0's 45 ms was an end-to-end process time, not the parse |
| `print-catalog` wall time | 45 ms (5 runs) | **26.1 ms** median (20 runs; hyperfine agrees at 25.6 ms) | -42% | not reproducible today on either binary; the section-0 figure is superseded |
| schema bytes | 7,168,658 B (62%) | 6,951,145 B compact (59.7%) | -3% | inside the band; differs by serialization method |

Side note, not the baseline: re-measuring with the direnv-built binary gave
startup 1925 / 1845 / 1843 ms (3 runs) and `print-catalog` 44 / 44 / 46 ms in
the team lead's hands, and 24.7 ms here with hyperfine. The codegen flags move
startup by nothing measurable, which is independent evidence that CPU work is
not where startup goes.

## 6. Predictions each later phase must confirm or falsify

- **P2** (auth + pruning off the critical path): startup median from 1809 ms to
  ~25 ms (the offline figure minus gcp_auth construction, i.e. the
  `print-catalog` envelope). The plan's "<60 ms" should be met by P2 alone.
- **P3** (session cache): dispatch loses one `initialize` round trip per call.
  Measured per-call upstream latency is not in this baseline; P3 must add the
  dispatch probe before claiming a delta.
- **P4** (search): 1,099 -> 0 allocations per query, the `"cloud run"` ranking
  fixed, and at most ~50 µs of latency removed; anything claiming 10x on search
  latency is measuring something else.
- **P5** (zero-copy snapshot): -10 ms of parse (not -45), -33 MB of startup
  allocation, and the binary payload from 12.6 MB of `__const` toward a
  compressed form; the "<13 MB" binary target is against 30.4 MB, not 25.3 MB.
- **P6** (fan-out): background, invisible in every number above; needs its own
  probe.
- **P7** (profile settings): on startup, zero measurable effect (section 5, side
  note); on binary size, `codegen-units=1` + dead-strip already shows -17%. P7
  either shows a number from this harness or is dropped.

## 7. Phase ledger

| Phase | Commit | Startup real median (ms) | Offline median (ms) | Search warm `cloud run` (µs / allocs) | Parse (ms) | Binary (B) |
|---|---|---:|---:|---:|---:|---:|
| baseline (P1) | `d341bbb` | 1809.31 | 137.80 | 58.89 / 1,099 | 9.86 | 30,442,048 |
| P2 | `b4b184d` + `7e7af67` | **22.84** | **22.78** | 58.64 / 1,099 (untouched) | 9.21 (untouched) | 31,586,464 |
| P3 | `26b0cd3` (+ addendum) | unchanged | unchanged | untouched | untouched | see section 9 |
| P4 | `b121b12` | untouched (index is lazy; offline min 22.42 vs P2's 22.78, section 10) | 47.64 under load avg 2.8-5.8 (control 55.74) | **67.70 / 0** | 9.21 (unchanged) | 31,684,224 |
| P6 | none (reverted) | untouched | untouched | untouched | untouched | untouched; fan-out 6.64 -> 6.45 -> 6.32 s (A/B/A', null -- section 11) |
| P7 | `924439d`, `f4767ae`, + this commit | **-0.82 ms** (profile, paired; 12a). mimalloc's -1.12 ms did not survive P5 -- re-measured 6/12, removed (section 14) | same probe (`--offline`) | untouched (`[profile.bench]` pinned, section 12) | untouched | **-6,688,864 from the profile**; mimalloc's +206,656 given back |
| P8 | none (declined) | untouched | untouched | untouched | untouched | -646,112 measured, **not adopted** -- section 17 |
| P5 | `8be5bd4` + `4bc3bb3` | offline paired min 20.3 -> **8.9 ms** (-56%), medians agree -- section 13 | same probe | within band under load (section 13c) | serve load 9.32 ms -> **89.5 us / 2,852 allocs**; JSON path kept | **16,472,160 (-34.6%)** |

Notes on the P2 row:

- Two-tier only. Flat mode (`--expose flat`) stays network-bound by design --
  median 1905 ms on the same harness (section 8a) -- because its tool list is
  fixed at `initialize` and there is no `listChanged`, so the exposed set must
  be final before serving. The alternative of serving the configured set
  unpruned and letting disabled APIs fail at call time with the
  `SERVICE_DISABLED` remediation was considered and rejected: it would list
  tools that cannot be used, and the remediation only helps after a failed
  call. Do not "fix" flat's startup without re-making that trade.
- Independent measurement by the team lead on `b4b184d`: time to the
  `tools/list` *response* min 18.77 / median 19.47 / p95 20.66 ms; time to
  process *exit* min 131.63 / median 133.36 / p95 136.90 ms. The exit figure
  is the background task finishing gcp_auth's ~113 ms trust-store load, which
  is why a `hyperfine -N` over a piped session reads ~130 ms: hyperfine
  measures to exit, not to first response.
- A failed or timed-out credential fetch is held for 30 s
  (`FETCH_FAILURE_COOLDOWN`); `list_services` keeps reporting `failed` with
  the original reason throughout, and the first call after the window retries.

## 8. P2 -- network off the startup critical path

Same harness, same machine, same flag regime as section 1. Binary:
`target/release/mcp-google-service`, 31,557,600 bytes, sha256
`fd033937a79799f99b18ed06b78d34a47c83dbfef0b3ab8aee76a95de13b5db2`, built
2026-08-20T15:33:01+0900 with `env -u RUSTFLAGS cargo build --release`.

What changed: `serve` parses the snapshot, answers `initialize` and
`tools/list`, and only then -- after rmcp has answered `initialize`, in one
background task -- acquires credentials (gcp_auth discovery is now lazy),
consults Service Usage (skipped under `--only`), narrows the exposed set, and
runs the live refresh. `--strict-startup` keeps the pre-P2 ordering;
`--expose flat` implies it. `list_services` reports per-service readiness plus
a `startup` block with the credential/enablement state and failure text.

| Mode | 20 serial runs | min | **median** | p95 | max | mean | baseline median | delta |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| real | `scripts/bench-startup.sh --runs 20` | 21.60 | **22.84** | 51.46 | 54.02 | 27.35 | 1809.31 | **-98.7% (79x)** |
| `--offline` | `--offline --runs 20` | 21.84 | **22.78** | 24.12 | 24.47 | 22.83 | 137.80 | -83.5% |
| real, `--strict` | `--strict --runs 20` (passes `--strict-startup`) | 1075.84 | **1785.55** | 2036.72 | 2071.27 | 1765.86 | 1809.31 | -1.3% (the old path, unchanged within noise) |

Per-run values, sorted:

- real: 21.60 22.07 22.33 22.41 22.52 22.59 22.65 22.73 22.74 22.82 22.87 22.92
  23.50 23.91 24.13 25.20 34.94 39.63 51.46 54.02
- offline: 21.84 21.85 21.94 22.09 22.18 22.20 22.42 22.56 22.60 22.73 22.83
  22.86 22.89 22.97 23.13 23.16 23.75 23.94 24.12 24.47
- strict: 1075.84 1243.74 1721.53 1740.20 1740.92 1746.65 1755.92 1764.62
  1773.30 1783.51 1787.58 1791.56 1793.91 1830.94 1840.84 1936.25 1938.69
  1943.21 2036.72 2071.27

Reading: real and offline now agree to within 0.1 ms, which is the proof that
nothing network-dependent is left on the path; what remains (~23 ms) is the
`print-catalog` envelope from section 1 (exec + dyld + a ~10 ms snapshot parse
+ assembly + the handshake). The revised P2 acceptance (median < 160 ms) is met
with a 7x margin; the plan's original `< 60 ms` is met as well, because the
gcp_auth root-store load moved off the startup path together with the network
calls (it is now paid by the background task, or by the first `call` if that
comes first). The real-mode p95 (51 ms) is wider than offline's (24 ms): in
real mode the background task's first step (the ~120 ms trust-store load)
starts right after `initialize`, and in four of twenty runs it overlapped the
`tools/list` response on the runtime.

What the caller sees between startup and readiness is documented in
`README.md` ("Startup: what is resolved when") and covered by
`tests/integration.rs` (`serve_answers_before_credentials_and_enablement_resolve_then_narrows`,
`only_never_consults_service_usage`,
`a_credential_failure_is_reported_by_list_services_and_returned_by_call`).

### 8a. Review follow-up: the blocking discovery, the cooldown, flat mode

Final P2 binary: 31,586,464 bytes, sha256
`a4793fb00cb8d8702a5c87951a2b758f4b888eabd1a27656a2d1a79dbc6228f5`, built
2026-08-20T15:55:35+0900 with `env -u RUSTFLAGS cargo build --release`.
Everything in this subsection was measured on it (the one-worker probe
"before" on its predecessor, which differed only by the `block_in_place`).

**A stall the first table hid.** Re-measuring while the machine was busy
(load average 5-10 from desktop applications) turned the real and offline
distributions bimodal (22 ms or ~50 ms). Pinning tokio to one worker
(`TOKIO_WORKER_THREADS=1`, offline, 10 runs) made every run slow -- min 124 /
median 162 / max 169 ms -- which isolated the cause: gcp_auth's credential
discovery does ~120 ms of *blocking* work (system trust-store load, key
parsing) inside an async fn, and the background task polled it on a runtime
worker right after `initialize`, holding that worker's queue, including the
MCP session's own tasks, until it finished. On sixteen workers that only
showed when the OS was slow to let another worker steal the queue; on one it
showed every time. Discovery now runs behind `tokio::task::block_in_place`
(`auth::discover_provider`), so the worker hands its queue over before
blocking; the same one-worker diagnostic afterwards: min 21 / median 52 /
max 56 ms, i.e. the 120 ms is gone and what is left is the machine.

**What is left is the machine.** The control for that claim: `print-catalog`
(no MCP session, no background task) timed through the same harness during the
same busy window gave min 24.00 / median 56.09 / max 61.16 ms, against 22.6 ms
+- 0.6 from hyperfine minutes earlier and 26 ms in section 1. The ~30 ms slow
mode is process-spawn and scheduling latency under load, paid equally by
everything the harness starts, and not a property of `serve`. Final numbers
below were taken with a `print-catalog` control run before and after, so the
environment they were taken in is on record:

| Run | 20 serial runs | min | **median** | p95 | max | mean |
|---|---|---:|---:|---:|---:|---:|
| control before: `--print-catalog` | load avg 8.8 | 24.18 | **54.54** | 58.69 | 59.44 | 46.00 |
| real (two-tier) | `--runs 20` | 28.69 | **51.50** | 55.30 | 56.73 | 47.80 |
| `--offline` | `--offline --runs 20` | 20.35 | **50.99** | 55.47 | 58.69 | 43.90 |
| real, `--flat` (`--expose flat`, implies strict) | `--flat --runs 20` | 1039.91 | **1905.14** | 2450.93 | 2583.45 | 1960.89 |
| control after: `--print-catalog` | load avg 8.0 | 22.93 | **56.47** | 59.08 | 60.05 | 50.14 |

Per-run values, sorted:

- real: 28.69 30.34 36.80 37.27 39.14 45.37 45.99 50.51 50.93 51.18 51.83 52.04
  52.69 52.70 54.18 54.29 54.69 55.24 55.30 56.73
- offline: 20.35 21.34 22.74 24.05 30.61 36.39 38.41 45.16 47.65 50.88 51.10
  52.32 53.06 53.20 53.90 54.11 54.29 54.30 55.47 58.69
- flat: 1039.91 1278.75 1799.77 1814.99 1842.26 1851.71 1870.11 1871.63 1883.05
  1904.61 1905.68 1915.52 1925.97 1958.83 2199.41 2283.28 2390.39 2447.63
  2450.93 2583.45

Reading: under this load, two-tier real (51.5), offline (51.0) and the
network-free control (54.5 / 56.5) are the same distribution -- the serve path
costs nothing beyond spawning a process here -- and the quiet-machine figures
in the first table (22.84 / 22.78 ms) remain the intrinsic numbers. The
acceptance (median < 160 ms) holds in both environments with a 3-7x margin.

Flat mode is measured separately because it is *not* expected to move:

Flat stays on the network-bound path by design: its tool list is fixed at
`initialize` and there is no `listChanged`, so the exposed set must be final
before serving. The alternative was considered and rejected: flat could serve
every configured service unpruned and let a disabled API fail at call time
with the existing `SERVICE_DISABLED` remediation, which would make it start as
fast as two-tier, but it would hand the client tools that cannot be used and
the remediation only helps after a failed call; listing unusable tools
degrades the model's view of the world. (The flat probe reads the multi-MB
`tools/list` line with `grep -m1` rather than bash's byte-at-a-time `read`,
which would otherwise add seconds of its own; the grep spawn is noise at this
scale.)

Failure-path guard added after review: a failed or timed-out token fetch is
held for 30 s (`FETCH_FAILURE_COOLDOWN`), during which every `call` returns
the same classified failure at once with a retry horizon instead of re-walking
gcp_auth's retry chain (~750 ms of back-off plus discovery, or the full 30 s
timeout for a wedged source); the first call after the window retries, so a
repaired credential still needs no restart. Unit-tested with a counting source
(`a_failed_fetch_is_not_retried_until_the_cooldown_passes`,
`a_repaired_source_is_picked_up_after_the_cooldown_without_a_restart`).

Not done here, on purpose: the gcp_auth native-root-store load (~120 ms) still
precedes the first `call`; replacing it with bundled roots is a security
decision for team-verify, not a P2 change. Binary grew by 1,115,552 bytes
(+3.7%) with the readiness machinery and the lazy credential source; size is
P5/P7's concern and is recorded, not optimized, here.

## 9. P3 -- one round trip per dispatch, not two

Before P3 every dispatch built a transport, ran a full MCP `initialize`, made
one `tools/call` and cancelled the session, so each call cost two round trips
to Google. Sessions are now cached per service, keyed by the token generation
their headers were built from.

**Acceptance (plan section 5.4) is a handshake count, not a timing.** The
streamable-HTTP client has no session id until `initialize` completes, so the
`initialize` POST is the one request that arrives without an `Mcp-Session-Id`
header. Counting header-less requests at the in-process upstream therefore
counts handshakes exactly, needs no request bodies, and counts a *failed*
handshake too (a 401 yields no session id).
`a_second_dispatch_reuses_the_session_and_does_not_reinitialize`: three
dispatches to one service, **1 handshake**.

**End-to-end, against real Google.** `tests/live.rs::live_second_dispatch_reuses_the_session`,
`run__list_services` on `https://run.googleapis.com/mcp`, project
gaudiy-licentia-dev, real ADC, two dispatches in one server process:

| Sample | cold (handshake + call) | warm (call only) | saved |
|---|---:|---:|---:|
| 1 | 1076.4 ms | 379.8 ms | **696.6 ms** (2.83x) |
| 2 | 1031.2 ms | 581.4 ms | **449.7 ms** (1.77x) |

Cold is stable at ~1.03-1.08 s; the warm figure carries Google's own per-call
latency, which is what varies between samples. The saving is the avoided
handshake round trip, so every call after the first to a given service lands
roughly 450-700 ms sooner. Startup is untouched: the same build reported
process-start to ready 19.16 ms and initialize to first response 334 µs.

**Profile, stamped late -- and the omission is what this correction is for.**
Every figure above is from a **release** build. The original text said only
"the same build", which is not reproducible: `tests/live.rs` resolves the
binary through `CARGO_BIN_EXE_*`, so it inherits whichever profile the *test
run* used, and README's documented live command carries no `--release`. A
later validation run against ship state compared a **debug** live run with the
release numbers above and read a 12x regression that does not exist. Measured
on `77acb16`, same tests, both profiles:

| Metric | section 9 (P3, release) | `77acb16` release | `77acb16` debug |
|---|---:|---:|---:|
| process start -> ready | 19.16 ms | **7.23 ms** | 54-56 ms |
| initialize -> first response | 334 µs | **502.75 µs** | ~4 ms |
| dispatch cold / warm / saved | 1076 / 380 / 697 ms | 1143 / 360 / 783 ms | 973 / 402 / 572 ms |

The release column **validates this section at ship state**; ready improved
19.16 -> 7.23 ms, which is P5's archive arriving where it should. The debug
column runs ~12x on first response and ~7x on ready, and is comparable only to
itself. **A latency figure without its profile is not a measurement** -- the
ratio is recorded here so the next reader converts instead of re-deriving it,
or worse, re-raising the same false alarm.

A contention explanation was tested *first* and falsified, which is the only
reason it was not written up as the cause: with every outbound connection
refused (`HTTPS_PROXY=127.0.0.1:1`, so the 47-host fan-out cannot start),
debug first response measured 3.88 / 4.10 / 3.75 ms -- indistinguishable from
online. The first response does not wait on background work. The handler is
also synchronous by inspection (`readiness()` is a snapshot, `service_label()`
a pure match, the payload a 47-entry map), so the time is round-trip and
codegen, not payload cost.

**Decomposed, so no component is left named-by-elimination.** Five
`list_services` calls in one session, driven over stdio as raw JSON-RPC (no
client-library overhead), both profiles:

| Profile | r1 | r2 | r3 | r4 | r5 | first-touch cost |
|---|---:|---:|---:|---:|---:|---:|
| release (shipped) | **0.256 ms** | 0.154 | 0.146 | 0.123 | 0.137 | **0.110 ms** |
| debug | 2.272 ms | 0.863 | 0.848 | 0.809 | 0.832 | **1.424 ms** |

Two mechanisms, both real, in the ratio that matters. A **first-touch cost**
exists -- work P5 moved off startup lands on the first call, which is the
design working as intended -- but it is 0.110 ms in the shipped profile. The
**steady state differs ~6x by profile** (0.14 ms release against 0.83 ms
debug), which is the profile explanation reappearing in a measurement that has
nothing to do with section 9. No per-call regression exists: r2 through r5 are
flat.

**Release r1 (0.256 ms) is faster than the 334 µs this section published**, and
r2 settles at 0.154 ms. P5 moved work from startup into first touch and the
first touch still came out ahead.

Bound on the claim: `list_services` reports counts and never touches tool
schemas, so this is the first-touch cost **on that path only** -- it is not a
measurement of P5's lazy schema inflation, which a first `describe_tools` or
`search` would exercise and which could be larger. Do not quote 0.110 ms as
the general cost of P5's laziness.

**Why this was ever comparable in the wrong direction:** the budgets are wide
enough to hide the very mismatch that produced them. A 100 ms budget passes a
334 µs release figure and a 4 ms debug figure alike, so the profile confusion
survived every green run until someone compared two numbers instead of
comparing a number to its limit. **Loose budgets are a defect class of their
own**: they do not merely fail to catch a regression, they actively conceal
the conditions under which their own reference numbers were taken.

**Standing rule, adopted from this correction: every published latency figure
carries its build profile.** A latency number without its profile is not
reproducible, and this ledger has now demonstrated the failure mode once.

Correctness of the cache, all asserted by test rather than by argument:

| Property | Test |
|---|---|
| A rotated token opens a new session rather than reusing the old one | `a_rotated_token_forces_a_fresh_session` |
| A handshake refused with 401 is retried once against a fresh token, and only once | `an_unauthorized_handshake_is_retried_once_against_a_fresh_token` |
| A session cached before a credential failure is dropped, and a cooling-down credential does not resurrect it | `a_credential_failure_drops_every_cached_session` |
| An idle session is closed and reopened after its TTL | `an_idle_session_is_reopened_after_its_ttl` |
| The cache is bounded, evicting least-recently-used | `the_session_cache_is_bounded_and_evicts_least_recently_used` |
| The generation changes exactly when a fresh token is cached, and a superseded invalidation is a no-op | `the_generation_changes_exactly_when_a_fresh_token_is_cached`, `invalidating_a_superseded_generation_is_a_no_op` |

**Deliberately not retried:** a failure on a *reused* session's call. That
error can arrive after the upstream has already run the tool, and the tools
reached here include `run__deploy_service_from_image`,
`compute__create_instance` and `compute__delete_instance`; retrying could
deploy or create twice. The session is dropped instead, and the next dispatch
rebuilds -- where a stale-token rejection arrives at the handshake, before any
tool runs, and is retried there. Bound is 16 sessions, idle TTL 5 minutes.

Gate note, for anyone re-running these at this commit. P4 was under way in
the same working tree during P3, so a whole-tree `nextest` here reports
failures that are not P3's: P4's golden-ranking tests are written red on
purpose, before the ranking rewrite they pin. The P3 gates were therefore run
in a separate git worktree containing this branch and nothing else --
`fmt --check` clean, `clippy --all-targets -- -D warnings` clean,
`nextest run` **148/148 passed, 0 skipped**, full suite with nothing excluded.
An earlier scoped run in the shared tree,
`nextest run -E 'not binary(search_ranking)'`, agreed at 147/147 before the
eviction test was added.

Two traps that cost time and would cost it again. `direnv exec .` does nothing
in a fresh worktree -- direnv refuses an `.envrc` it has not been told to
allow at that path, so the shell's nightly `RUSTFLAGS` pass through and cargo
fails with "the option `Z` is only accepted on the nightly compiler"; use
`env -u RUSTFLAGS` there, or `direnv allow` the path. And a worktree needs its
own `build.target-dir` (`--config 'build.target-dir="..."'`): the binary path
under `target/` is fixed, so two checkouts otherwise collide and serialize on
the same build lock, which is the interference the worktree exists to avoid.

## 10. P4 -- search: ranked by what the query names, allocating nothing

Commit `b121b12`. Same machine and flag regime as section 2
(`env -u RUSTFLAGS cargo bench --bench bench_search`, divan, timer precision
41 ns, serial, worker-1 confirmed idle for the whole window). Endpoint: every
number in this section is an in-process micro-benchmark over the embedded
47/548 snapshot, except the startup probes at the end, which are
process-start -> `tools/list` over stdio via `scripts/bench-startup.sh` on a
fresh `env -u RUSTFLAGS cargo build --release` binary (31,684,224 bytes,
sha256 `ac26ba3a...`, +0.3% over P2's: the index code and the score fields).

What changed: the catalog builds a search index on first use (every
searchable string lowercased once into one arena; CamelCase names also
indexed as `_`-separated words), `search_with(query, filter, limit, visit)`
ranks against it with a stack-held query, a reused thread-local candidate
buffer and `select_nth_unstable` top-k instead of a full sort (P1 finding
#5), and the scorer now sees service ids: a run of query tokens that spells
one (a leading `cloud` being brand, not id: Google publishes `run` but
`cloudtrace`) outranks name-substring accidents like `cloud` inside
`gcloud`. `search_tools` passes its limit down and reports `score` and
`total_matches` (lead-approved surface ruling). The 2-arg `search` stays as
an allocating convenience wrapper, which is what `warm_collected` measures.

### 10a. Allocations per query (the acceptance criterion)

divan `AllocProfiler`, per iteration. Baseline figures are section 2a
(`d341bbb`, the same wrapper `warm_collected` still measures).

| Row | cloud run | instances | miss (`zzzznomatch`) |
|---|---:|---:|---:|
| baseline `warm` (= the wrapper) | 1,099 / 275.7 KB | 1,099 / 275.7 KB | 1,098 / 275.6 KB |
| **P4 `warm` (serve path, limit 20)** | **0 / 0 B** | **0 / 0 B** | **0 / 0 B** |
| P4 `warm_unbounded` (full ranking) | 0 / 0 B | 0 / 0 B | 0 / 0 B |
| P4 `warm_collected` (wrapper: result `Vec` only) | 4 events / 256 B peak | 5 events / 512 B peak | 0 / 0 B |
| P4 `warm_filtered_to_run` | 0 / 0 B (baseline: 13 / 2.4 KB) | | |
| P4 `warm_empty_query` | 0 / 0 B (baseline: 2 / 8.8 KB) | | |

Acceptance "zero allocations on the query path" is met on the path the
server actually calls, and even when the entire ranking is delivered. What
remains in the wrapper is the returned `Vec` itself. The cost moved to
construction, where it is paid once per catalog instead of per query: `cold`
(first search on a fresh catalog) is 3 allocations / 326.7 KB -- the index
arena plus its two span tables -- and 105-139 us total, i.e. ~70 us of index
build on top of the query. A catalog is created twice per process (startup
assembly, live-refresh swap), and the build is lazy, so startup never pays
it; the first `search_tools` after a swap does.

### 10b. Latency (not the target; one regression, declared)

Warm medians, P4 vs section 2a. All under the plan's 100 us line.

| Query | baseline | P4 | delta |
|---|---:|---:|---:|
| `instances` | 47.47 us | **37.20 us** | -22% |
| `zzzznomatch` (miss) | 48.52 us | **34.04 us** | -30% |
| `list cloud zzzznomatch` (miss) | 59.11 us | **56.24 us** | -5% |
| `cloud run` | 58.89 us | **67.70 us** | **+15%** |
| `list cloud run` | 57.56 us | **62.99 us** | +9% |
| `list` filtered to `run` | 407.9 ns | **392.9 ns** | -4% |
| empty query | 3.94 us | **3.54 us** | -10% |

The regression on multi-token hit queries is real and expected: the scorer
now walks name words, spells service ids and verifies adjacent-token
phrases, which costs more per candidate than the removed per-tool
lowercasing saved on those two queries. It is declared rather than reverted
because ranking correctness is the phase's point, the plan's latency line
(<100 us) holds with 32% headroom, and per-query time is 68 us against
Google's 1-6 s tool latencies. If a later phase wants the 8 us back,
`memchr::memmem` on the description scans is the first candidate; it was
not adopted here because no target required it.

Parse is untouched (re-measured: `parse_embedded` 9.21 ms vs 9.86 baseline;
`assemble_serve_catalog_all_endpoints` 219.8 us vs 270.6 -- run-to-run
variance, no code on that path changed).

### 10c. Ranking (the correctness defect)

`tests/golden/search-ranking.txt` pins the head of fifteen rankings;
`tests/search_ranking.rs` checks them plus the properties that justify
them. Written and run RED against `3f72987` before the rewrite: 3 of 10
tests failed, 7 of 15 blocks diverged. The E2E case as recorded red, query
`cloud run`: actual head was `cloudcli__run_gcloud_command,
cloudcli__run_bq_command, alloydb__export_data, alloydb__import_data,
bigquery__execute_sql_readonly, ...` with `run__*` at ranks 12-16; `run`
led with `bigquerydatatransfer__*_transfer_run`; `pubsub topics` returned
zero hits (no description spells "pubsub"). After the rewrite all 10 pass:
`cloud run` returns exactly Cloud Run's five tools first, then the gcloud
escape hatch; `pubsub topics` resolves via the service id. Full suite
168/168 (was 139 at baseline; the golden tests, the scorer unit tests and
the payload-shape tests are the growth, alongside P3's).

Notes for later phases and team-verify:

- `benches/bench_search.rs` (P1's instrument) was modified because the API
  under measurement changed; the profiler, query set and cold/warm split
  are unchanged, and `warm_collected` measures the same call the baseline
  did, so the before/after above is apples-to-apples.
- `src/server.rs` was touched under an explicit lead ruling: the
  `search_payload` hunk, the `search_tools` description, and two payload
  tests. Ranking goldens did not change for it (additive fields only).
- Still open, reported not fixed: `first_line()` truncates descriptions
  mid-sentence (server.rs, E2E finding #2).
- P5 interaction: the index is derived data behind a `OnceLock` -- a clone
  or a snapshot round-trip starts without it and rebuilds lazily, so the
  rkyv migration does not need to serialize it; `NamespacedTool`'s wire
  shape is unchanged (`#[serde(skip)]` on the catalog's index field).

## 11. P6 -- the HTTP/2 one-off: measured, null, reverted

Per the W4 ruling P6 was reduced to one question -- does enabling reqwest's
`http2` feature (suppressed today by `default-features = false`) move the
47-host discovery fan-out? -- with the phase dropped either way, its other
bullets having been eaten by P2 (refresh after serve) and P3 (session reuse,
shared pool).

Probe: `snapshot --out /tmp/...` (the full unauthenticated 47-host fan-out;
process-exit endpoint; a partial fan-out exits non-zero and would invalidate
the run -- all runs completed 47/47). hyperfine, `RUSTFLAGS` unset, fresh
release build per arm, A-B-A so drift between arms cannot pose as the
feature. Decision rule fixed before measuring: keep only if B moves >10%
AND clears the wider arm's spread.

| Arm | Binary | mean +- sigma | min .. max | runs |
|---|---|---:|---:|---:|
| A: HTTP/1.1 (HEAD) | `ac26ba3a...` | 6.643 s +- 0.383 | 5.905 .. 7.026 | 6 |
| B: + `http2` | `a272adeb...` | 6.453 s +- 0.305 | 5.946 .. 6.783 | 6 |
| A': HTTP/1.1 again | `ac26ba3a...` | 6.319 s +- 0.556 | 5.781 .. 6.892 | 3 |

That B actually spoke HTTP/2 was verified, not assumed: `RUST_LOG=h2=debug`
on the B binary logged 1,727 `h2::` frames during one fan-out; the same
probe on the reverted binary logged zero. So the comparison is real and the
answer is null: A-to-B is -2.9% while A-to-A' is -4.9% with no change at
all -- the network's own drift exceeds the feature's effect, exactly the
revert rule's case.

Why it cannot help here, mechanically: the fan-out contacts 47 *distinct*
hostnames, one connection each; HTTP/2 multiplexes streams within one
connection and hyper does no cross-host origin coalescing, so there is
nothing for it to multiplex. The sequential initialize -> tools/list on a
warm connection costs the same round trips on either protocol. Where h2
*could* matter is many concurrent calls to one host through P3's cached
session -- that session is rmcp's transport, not this client, and no
measured workload does that today.

Reverted: `Cargo.toml` and `Cargo.lock` are byte-identical to `b121b12`'s;
nothing landed on the code. **P6 is closed.** If a future change makes
per-host concurrency real, re-run this section's probe rather than assuming
either outcome.

## 12. P7 -- build settings: a size phase, as predicted

Commit `53a20c2` plus this one. Same machine and toolchain as section 1
(rustc 1.97.1). Size is deterministic, so these arms need no measurement
window; each was built into a **fresh** target directory, because codegen and
link flags do not reliably force a relink of a cached binary and a stale
artifact would attribute bytes to the wrong flag.

| Arm | total | Δ vs base | `__text` | `__LINKEDIT` |
|---|---:|---:|---:|---:|
| base (no flags) | 31,683,456 | -- | 9,787,284 | 6,209,536 |
| `codegen-units = 1` | 25,811,648 | **-5,871,808** | 7,619,104 | 3,620,864 |
| `-Wl,-dead_strip` | 31,683,456 | **0** | 9,787,284 | 6,209,536 |
| `cu=1` + `lto = "thin"` | 26,051,696 | -5,631,760 | 8,166,016 | 3,358,720 |
| **`cu=1` + `lto = "fat"`** | **24,994,592** | **-6,688,864 (-21.1%)** | 7,739,040 | 3,047,424 |

Adopted: `codegen-units = 1` and `lto = "fat"` in `[profile.release]`. The
built binary works (`print-catalog` returns 47 services / 548 tools).

**The exact byte count is reproducible only for a given pair of paths**, which
is worth stating because two "exact" figures for the same commit would
otherwise look like a contradiction. `strip = "none"` keeps DWARF, and DWARF
embeds absolute build paths, so the total moves with the *length* of the
source and target-dir paths. Three builds of identical source:

| Source dir | Target dir | bytes |
|---|---|---:|
| `...-p3` (worktree) | `/Volumes/tmpfs/target-p7` (24 ch) | 24,994,592 |
| `...mcp-google-service` | `/Volumes/tmpfs/target-reconcile` (31 ch) | 24,994,736 |
| `...mcp-google-service` | `<repo>/target` (61 ch) | 24,995,424 |

Monotonic in path length and consistent in magnitude: the 30-character
target-dir difference in the last row costs 688 B, the 7-character difference
in the first costs ~144 B, i.e. roughly one path copy per compilation unit.
So the 832 B spread across this section's builds is explained, not noise, and
any future byte-for-byte comparison has to hold both paths fixed.

**Mechanism, corrected.** An earlier draft of this section blamed DWARF. It is
not DWARF: the linked Mach-O has **no `__DWARF` segment at all** (`size -m`
lists only `__PAGEZERO`, `__TEXT`, `__DATA_CONST`, `__DATA`, `__LINKEDIT`).
On this target the linker leaves debug info in the `.o` files and writes a
*debug map* into the symbol table instead -- **24 `N_OSO` stabs, each carrying
an absolute `.o` path**, which `nm -ap` shows pointing straight at the
target-dir (`.../target-alloc/release/deps/...`). That is what moves with path
length, and the arithmetic then lands: 24 entries x 30 characters = 720 B
predicted against 688 B observed. The "one path copy per compilation unit"
reading was right; the thing being counted is OSO entries, not DWARF DIEs.

**Confirmed out of sample, and by direct count.** The rate above was derived
on a 25 MB binary. The shipped 16 MB binary at `77acb16` was built twice from
identical source into target dirs of 24 and 61 characters: **16,096,384 B and
16,097,184 B, a 800 B difference across 37 characters**, against 849 B
predicted by this section's rate and 888 B by the naive 24 x 37. A 35% smaller
artifact pays the *same absolute* cost, which is what "entry count x path
length" predicts and what any proportional or DWARF-sized story does not.
`nm -ap` on that binary counts **exactly 24 `OSO` entries**, so the constant
is now measured rather than inferred. Two consequences: the mechanism is
load-bearing, and **no size row in this ledger is meaningful without its
target-dir path** -- two different correct byte counts for one commit is the
normal case here, not a contradiction.

`strip = "none"` is still why they survive. Release link time goes from ~40 s to ~112 s; the
test profile inherits dev, so `nextest` pays none of it.

**`lto = "thin"` is worse than no LTO at all here** -- the obvious "safe
middle" choice would have shipped a **+240,048 B regression** against
`codegen-units = 1` alone. It trades 546,912 B more `__text` for 262,144 B
less `__LINKEDIT`; `"fat"` makes the opposite trade (+119,936 `__text`,
-573,440 `__LINKEDIT`) and wins. Nobody should adopt `"thin"` here on the
reasonable-sounding grounds that it is the moderate option.

**`dead_strip` is a structural null, not an observed one.** It removed zero
bytes from every section, and the mechanism is that `rustc` already passes
`-Wl,-dead_strip` on every Apple link: `rustc -O --print link-args` on the
pinned **1.97.1** contains exactly one occurrence, so the explicit flag was a
duplicate. That is why the totals were byte-identical while the sha differed: ld64
hashes the link command line into `LC_UUID`, so the recorded identity of the
build changed while the emitted code did not. It cannot buy a byte on
this target under any invocation shape, so the question of how to deliver it
(ambient `RUSTFLAGS`, a committed `.cargo/config.toml`, a `build.rs` emitting
`cargo::rustc-link-arg-bins`, or a separate release command) is moot, and the
crate keeps its "no build scripts" property.

This also explains the earlier -5.18 MB figure in section 3 as a **confound**:
that comparison was against a build carrying the entire direnv set
(`target-cpu=native`, `opt-level=3`, `lto=thin`, and more), and the two flags
it was attributed to were simply the two that had been named.

**Positive control, run even though adoption no longer depended on it.** The
ambient `dead_strip` arm changed the hashes of **all 17 proc-macro dylibs**.
So the contamination mechanism behind `ae92467` is demonstrated rather than
feared: an ambient link flag does reach proc-macro dylib links. Had a link
flag been needed, `cargo::rustc-link-arg-bins` would have been the right shape
precisely because that equality check would then have meant something.

**Why `[profile.bench]` is pinned.** It inherits `[profile.release]`, so the
settings above would otherwise reach every benchmark binary -- measured,
`cargo bench --no-run` went from ~40 s to 110 s -- and the divan numbers in
sections 2a, 10 and here would have been taken under a profile that no longer
exists. The benches are the *differential* instrument: their worth is
comparing this crate's code across phases, which requires the profile beneath
them to hold still, and P5 is still to be judged against them. The *decisive*
instrument for a phase ruling is `scripts/bench-startup.sh` against the real
release binary, which measures the shipped configuration by construction. So
the shipped profile diverges for size while the diagnostic one stays frozen.

**Method note.** The first `lto` arms were driven through `RUSTFLAGS`, where
`-C lto` is incompatible with the `-C embed-bitcode=no` cargo passes to
dependencies. Both builds died instantly -- and because the harness redirected
stderr to `/dev/null`, they reported as `build=0s` with empty sizes: shaped
like data, not like failure. It was caught only because a zero-second release
build is impossible. A harness that renders failures as empty results is how a
wrong number gets archived; the arms were re-run through
`--config 'profile.release.lto=...'`, which is also the shape actually adopted.

### 12a. Startup: both pre-registrations falsified, both favourably

Both predictions were recorded *before* measuring; both were wrong, and both
were wrong in the direction that flatters the change, which is exactly when a
pre-registration has to be honoured loudest.

**The machine forced the design.** The desktop sat at load 8-13 for the whole
window (WindowServer and Chrome, nothing of this project's), and the section-1
control confirmed the regime: `print-catalog` median **40.62 ms** against its
quiet **26.12 ms**. Comparing against the quiet-machine 22.84 ms reference
would have measured the desktop. So nothing here is reported against a
historical number. The pre-P7-profile binary (`codegen-units = 16`,
`lto = false`, 31,740,336 B) was rebuilt and every comparison is **same-session
paired alternation**: the two binaries measured minutes apart under identical
conditions, `--offline` (lower variance, and startup is a CPU and dyld
question, not a network one).

A first attempt used coarse A-B-A and **was rejected by its own brackets**: the
two "new" arms disagreed by 11.34 ms (31.83 then 43.17), more than the A-vs-B
gap they were supposed to bound, so those medians were measuring drift.
Shortening each arm and alternating more often collapsed it.

| Arm | Predicted | Measured | Evidence |
|---|---|---|---|
| A: profile (`codegen-units=1` + `lto="fat"`) | **0** | **~0.82 ms faster** | minima 8/8 rounds, mean +0.82, median +0.74 (sign test p = 0.0039); medians 6/8, +0.50 |
| B: mimalloc as `#[global_allocator]` | **0** (author-flagged as weak) | **~1.1-1.5 ms faster** | minima 10/12, mean +1.12, median +1.05; medians 10/12, mean +1.51, median +1.20 |

**Arm A's mechanism was in the author's own notes and left out of the
prediction anyway**: the binary is 6,744,912 B smaller, so dyld maps less. The
prediction reasoned about codegen quality and forgot the size effect that the
W4 decision table had already named. ~3.9% of a ~21 ms floor.

**Arm B was nearly reported as a result when it was a coin-flip.** The first
run (8 rounds, heavier load) gave minima 7/8 **+1.23 ms** but medians 5/8
**-0.37 ms** -- the two statistics disagreeing. That was not reported; it was
re-run at 12 rounds once load settled, where both agree. Best single runs:
system 21.46 ms, mimalloc **19.52 ms**. The flagged doubt was right and the
prediction wrong: a 260,841-allocation parse burst is where an allocator shows.

**Derived, not measured:** applying both paired deltas to the quiet-machine
reference gives **22.84 -> ~20.9 ms**. No run produced that number directly;
the deltas were measured under load and the reference was not.

Binary with mimalloc: **25,200,624 B**, +205,200 over the allocator-free build,
so the phase nets **-20.5%** from 31,683,456 rather than -21.1%.

**Trust base.** mimalloc compiles a C library into a binary that handles
credentials. That is flagged for the security reviewer rather than assumed
free, along with the question of whether mimalloc's secure mode (guard pages,
encrypted free lists) earns its cost -- the numbers above are default mode, so
the reviewer can price the option against a measured baseline. **`zeroize` is
unaffected**: the token buffer is scrubbed *before* being freed, so which
allocator reclaims the page afterwards does not change what is left in it.

**Superseded by section 14**: mimalloc was removed after P5 eliminated the
allocation burst that justified it. Arm B's numbers below stand as measured --
they were correct about the binary they were measured on -- but they no longer
describe the shipped one.

## 13. P5 -- the catalog as a checked archive, schemas opened on demand

Commits `8be5bd4` (archive format, committed `data/catalog-snapshot.bin`,
identity/corruption/version tests) and `4bc3bb3` (serve from the archive,
schemas as per-tool zstd frames inflated once on first use). Both arms built
in the same worktree (`.../mcp-google-service-p5`, target-dir
`/Volumes/tmpfs/target-p5`), so sizes and pairings share one path per the
section 12 caveat. `RUSTFLAGS` unset throughout.

Predictions were registered with the lead before any measurement; each is
scored below.

### 13a. Startup (decisive; paired alternation, P-I)

`scripts/bench-startup.sh --offline --bin <arm> --runs 10`, arms alternated
A,B,A,B,A,B minutes apart while the machine ran load averages 6.9-15.7 --
which is why medians wobble and minima are the stable pair, exactly the
regime section 12a established.

| Round | main `f4767ae` (min / median) | p5 `4bc3bb3` (min / median) |
|---|---:|---:|
| 1 | 20.30 / 39.72 | 8.88 / 23.68 |
| 2 | 20.51 / 21.99 | 8.84 / 9.29 |
| 3 | 19.87 / 21.45 | 8.86 / 10.65 |

Minima: 20.3/20.5/19.9 against **8.9/8.8/8.9** -- a paired **-11.4 ms
(-56%)**, stable while the load average doubled. The quiet-round medians
(21.99 vs 9.29) agree with the minima, which is the standing bar.

**P-I scored: direction right, magnitude wrong -- the win is larger than
predicted.** Registered: -5.0 to -8.0 ms, landing at 13-15.5 ms. Measured:
-11.4 ms, landing at ~8.9 ms, through the plan's ~13 ms target. The
prediction accounted for removing the 9.2 ms parse (minus mimalloc's already
-banked ~1.1 ms) and adding ~0.5-2 ms of validation+materialization; it
missed two mechanisms, named here post-hoc and honestly as such: parsing
touched all 11.6 MB of the embedded JSON (~2,900 page-ins now gone -- the
archive touches ~2.2 MB and materializes ~0.5 MB), and dyld maps 8.7 MB less
binary, which section 12a already measured at ~1 ms for a 6.7 MB reduction.
The falsified half is the estimate of what remained after the parse: the
serve floor under P2's 22.8 ms quiet reference was not ~13 ms of
irreducible work, it was ~9.

### 13b. The numbers behind it (frozen bench profile, P-III/P-IV/P-V)

`env -u RUSTFLAGS cargo bench --bench bench_snapshot_load`, medians of 30:

| Row | median | allocations | predicted bound |
|---|---:|---:|---|
| `parse_json_into_catalog` (the old serve load; still the `--snapshot` path) | 9.32 ms | 261,406 / 33.27 MB | unchanged -- holds |
| **`materialize_embedded` (the new serve load)** | **89.54 us** | **2,852 / 538.8 KB** | P-III: <2 ms, <6,000 -- **104x under the old load** |
| `access_file` (checked validation alone) | 23.16 us | 774 KB transient | P-V: <0.5 ms -- holds 21x over; `access_unchecked` is not worth discussing |
| `describe_first_touch_one_tool` | 6.48 us | 14.2 KB | P-IV: <300 us -- holds 46x over |
| `assemble_serve_catalog_all_endpoints` | 111.9 us | | was 270.6 us at baseline |

### 13c. Size (P-II) and search parity

Binary, same path both arms: 25,200,624 -> **16,472,160 bytes
(-8,728,464, -34.6%)**; `__TEXT,__const` 12.6 MB -> 3.08 MB (the 11.64 MB
JSON replaced by the 2.22 MB archive). **P-II scored: hit** -- registered
15.5-16.5 MB, landed at the top edge. The committed archive is 2,220,096
bytes; per-tool zstd-19 with content checksums compresses the 6.95 MB of
compact schema JSON about 5.2x, inside the predicted 4-6x.

`bench_search` on the P5 branch under the same load: warm `cloud run`
71.89 us / `instances` 39.49 / misses 34.99 and 59.62, all with the alloc
sections absent (still zero on the query path). Against section 10's 67.7 /
37.2 / 34.0 / 56.2 that is +2-6% with slowest samples 60% over the fastest
-- the load, not the change; the search code is untouched and the golden
suite passes against the archive-materialized catalog.

### 13d. What the identity tests caught before measurement did

Two defects died in the suite rather than in the field, which is the reason
those tests exist and worth recording: (1) the first committed archive was
generated by a stale pre-checksum binary; the archive/JSON identity test
refused it. (2) `ToolSpec`'s serializer round-tripped annotations through
`serde_json::Value`, whose sorted maps silently reorder keys -- same byte
count, different bytes; the byte-identity test (materialized catalog ->
`to_json` == the committed 11,637,929 bytes exactly) caught it and forced
typed serialization through `rmcp::model::ToolAnnotations`.

Notes for team-verify and later work:

- Trust boundary (F1): operator `--snapshot PATH` files remain JSON through
  serde; foreign bytes never reach rkyv. The embedded archive is validated
  (bytecheck) anyway; corruption at any payload byte is a clean
  `ArchiveError::Invalid` (full-sweep test), a flipped bit inside a zstd
  frame -- invisible to structural validation -- dies at the frame's content
  checksum, and a version-tagged header refuses stale artifacts by message.
- `zstd` adds a C library (zstd-sys) to the trust base, same review posture
  as mimalloc's C; rkyv is pure Rust (MSRV 1.81).
- Flat mode inflates every schema at `initialize` (its list carries them);
  that is the 1.9 s network-bound path by design, and the inflate cost is
  ~548 x ~7 us. Two-tier never pays it. A background refresh comparison
  (`drift_from`) inflates and caches as it compares; worst-case memory
  equals the pre-P5 resident catalog.
- `print-catalog` still reads the working tree's JSON and falls back to the
  embedded archive, so its report follows the reviewed file, not the binary.
- The `snapshot` subcommand writes `.bin` beside `--out` and gains `--from
  JSON` (fan-out skipped, `generated_at` preserved, byte-identical JSON
  re-emit proven on the committed file), which is how the committed pair is
  regenerated together; the identity test makes landing them apart
  impossible.


## 14. P7 revisited -- mimalloc removed after P5 moved the ground

Section 12a adopted mimalloc on one stated mechanism: *"loading the snapshot
is a burst of 260,841 allocations, which is exactly the shape a
general-purpose allocator is beaten on."* Section 13b then recorded that P5
replaced that load with `materialize_embedded`: **261,406 allocations ->
2,852**, a 98.9% reduction. The justification had been invalidated by this
project's own ledger, so the adoption was re-measured rather than left to
stand on a mechanism the code no longer has.

**Pre-registered before measuring** (as in 12a, and this time correct): the
delta would fall inside the band on both statistics, and a survival claim
would require both agreeing in sign at >= 10/12 rounds -- the same bar 12a
used, so the adoption could not be rescued on a weaker standard than the one
that established it.

Same protocol as 12a Arm B: both arms built back to back at `4cee379` from one
source path into one target-dir, differing only in the `#[global_allocator]`
item and the dependency (16,472,224 B with, 16,265,568 B without -- a 206,656 B
difference matching P7's +205,200, which is itself a check that the arm
isolates mimalloc and nothing else). Both answer `print-catalog` 47/548.
12 alternating rounds, `--offline`, bracketed by `print-catalog` controls that
agreed (min 21.10 and 21.00 ms).

| Statistic | mimalloc faster | mean | median |
|---|---|---:|---:|
| minima | **6/12** | **-0.39 ms** | +0.00 ms |
| medians | **4/12** | **-1.15 ms** | -0.63 ms |

Best single runs: without **7.69 ms**, with 7.83 ms.

**Collapsed.** 6/12 on minima is a coin flip, and both point estimates lean
*negative* -- the shipped binary was, if anything, marginally slower with
mimalloc than without. Removed: dependency, `#[global_allocator]` item, and
206,656 B of binary.

What this costs and what it buys: the startup win from 12a is not lost, it
was never real on this binary -- it belonged to a load that P5 deleted. What
is bought is a **C library out of the trust base of a credential-handling
binary**, which retires the security question rather than answering it, and
which is the cheapest possible outcome for that review.

The rule this follows is the plan's own -- *a phase whose end-to-end delta is
inside the noise band is reverted, not kept* -- applied to a justification
rather than to a change. The adoption was correct on the evidence available
when it was made, and is reverted because the ground moved under it. Both
statements are true at once, and the second does not retract the first.

### 14a. Which endpoint this verdict rests on, and which burst it does not cover

**The endpoint is time-to-first-response, offline.** That is deliberate and it
is the same endpoint section 12a used to justify the adoption, so a collapse
verdict on it is decision-grade for the decision actually being revisited. It
is not a claim about every allocation this process makes.

**The burst is 98.9% gone at that endpoint. It partially returns about a
second later, in live two-tier mode.** `server.rs`'s background refresh calls
`Catalog::drift_from`, whose `schemas_equal` calls `input_schema()` and
`output_schema()` on *both* sides of every comparison -- inflating ~548 tools'
zstd frames on each side, post-handshake. Section 13's own notes already
record it ("a background refresh comparison (`drift_from`) inflates and caches
as it compares; worst-case memory equals the pre-P5 resident catalog"). The
offline protocol used here never fires it: no network, so no live refresh, so
no `drift_from`.

So an allocator could still matter for **background throughput and peak
memory** on the live path. That was never what mimalloc was adopted for --
section 12a measured startup latency and nothing else -- so it is out of
scope here rather than unmeasured-and-ignored.

**Consequence, and it is the section 9 endpoint rule appearing a second
time**: on this tree, time-to-first-response and time-to-process-exit will
*disagree*, and the disagreement is real rather than noise. The process cannot
exit until the background refresh finishes, and that refresh now carries the
inflation burst. A probe that times to exit is measuring a different question
than a probe that times to first response, and neither answer transfers to the
other.

**If an allocator is ever re-proposed, it must name which burst it targets**:
the startup burst (gone -- 261,406 -> 2,852, section 13b) or the background
`drift_from` inflation (extant, off the critical path). A proposal that does
not distinguish them is re-running this measurement against the wrong endpoint.

Startup after removal, for the record: minima cluster **7.7-8.8 ms**, medians
**9.1-15.3 ms** under a load average of ~4 (the controls above bound the
environment). Against P1's 1809.31 ms real-mode median that is the cumulative
result of P2, P3, P5 and P7's profile settings; no allocator is involved.

## 15. Credential shapes: the impersonation check

v1 exercised exactly one credential shape -- `authorized_user` ADC -- and
recorded service-account and workload-identity flows as unverified. The
service-account half is now closed, by the reviewer's Ruling 2 protocol, with
**no key file synthesized**: org policy forbids them, so the check ran against
an `impersonated_service_account` ADC wrapper built from the operator's own
ADC as `source_credentials`.

**Source reading first, prediction registered before measuring.** `gcp_auth`
0.12.7 has no impersonation support by construction: no
`impersonated_service_account` variant, no `service_account_impersonation_url`
handling anywhere in its `provider()` chain. Predicted: both placements fail,
neither yields a token, the `GOOGLE_APPLICATION_CREDENTIALS` placement fails
*immediately at parse* while the ADC-path placement falls through the chain
and fails later. **Confirmed on all three points.**

| Run | Placement | Exit | Wall | Terminal error |
|---|---|---:|---:|---|
| 1 | `GOOGLE_APPLICATION_CREDENTIALS` | 1 | **15 ms** | failed to deserialize ApplicationCredentials: missing field `private_key` |
| 2 | ADC path, `gcloud` **off** PATH | 1 | **931 ms** | `no available authentication method found` |
| 3 | ADC path, `gcloud` **on** PATH | 1 | **1359 ms** | same |

**Discharge: D1 (pass).** Both mandatory runs fail fast with actionable text,
non-zero exit, no panic, no hang (worst case 1.36 s against a 30 s ceiling),
and no token material -- `Bearer `/`ya29.`/`refresh_token`/`client_secret`/
`access_token`/`private_key` scanned across every captured stderr: **0 hits**.
`serving from snapshot ...` printed **zero times** in all three, which is the
end-to-end witness that no token was ever held: since the strict path acquires
credentials before serving, that line is unreachable on a credential failure.

**Run 3 is inconclusive, and the protocol is why that does not matter.**
It failed at `gcloud` -- but because this machine's `gcloud` CLI credentials
are expired, not because `gcp_auth` rejected impersonation. On a workstation
with a live `gcloud`, run 3 would likely have *succeeded* via
`GCloudAuthorizedUser`, minting a token that has nothing to do with
impersonation. That false pass is exactly the hazard the reviewer anticipated
by making run 2 -- `gcloud` genuinely off PATH -- the mandatory one. The
finding rests on run 2.

**Method notes, because two of them are traps.** `command -v gcloud` resolves
a *shell alias* here, so a first attempt at run 2 computed an empty PATH entry
and left `gcloud` reachable; the mandatory condition was unmet and the run was
redone after verifying a *subprocess* could not resolve it. And `gcp_auth`
locates ADC through `env::home_dir()`, **not** `CLOUDSDK_CONFIG`, so runs 2
and 3 redirected `HOME`; the operator's real ADC was never moved or written.
The wrapper embedded a live refresh token and was deleted immediately after,
deletion verified.

**Still open, and not to be read as cleared:** WIF (`external_account`) was
never exercised. Recording it as unsupported would repeat the run-3 error in
the opposite direction. What is established is narrower and exact:
**`impersonated_service_account` is unsupported by `gcp_auth` 0.12.7 and fails
clean.**

The failure text is nonetheless misleading in a way worth knowing: a user who
ran `gcloud auth application-default login --impersonate-service-account=...`
-- the org-policy-friendly path -- is told a `private_key` field is *missing*
from a credential they deliberately created *without* a key, which points them
at the one artifact policy forbids.

## 16. Live tier at ship state (`77acb16`)

Six tests, real Google endpoints, real ADC, project gaudiy-licentia-dev.
**6/6 passed.** Serial (`--test-threads=1`) and `RUSTFLAGS` unset: three of the
six assert latency budgets, and six concurrent servers each fanning out to 47
hosts would measure those under self-inflicted contention.

| Test | Result | Evidence emitted |
|---|---|---|
| `live_background_catalog_refresh_completes_within_its_budget` | **PASS** 5.353 s | `catalog refresh fan-out: 5.312124208s for 47 services (47 live, 548 tools)` |
| `live_developerknowledge_search_documents_dispatches_without_error` | **PASS** 2.348 s | `returned 1 content block(s)` |
| `live_run_list_services_dispatches_without_error` | **PASS** 1.068 s | `returned 1 content block(s)` |
| `live_second_dispatch_reuses_the_session` | **PASS** 1.410 s | cold 973.382041 ms / warm 401.527042 ms / **saved 571.854999 ms** |
| `live_snapshot_parse_stays_within_its_budget` | **PASS** 0.027 s | `snapshot parse: 15.351792ms for 47 services / 548 tools` |
| `live_startup_and_first_tool_response_meet_their_latency_budgets` | **PASS** 0.193 s | ready 25.13675 ms / first response 16.970875 ms |

**A green run here is not evidence, and was not accepted as such.** Every test
opens `let Some(project) = live_project() else { return; }` -- a bare `return`,
which the runner scores as a pass. An unset gate therefore produces six green
lights and zero live calls, which is the same picture as success. The run was
repeated under `--no-capture` and only *past-the-gate* output is recorded
above: `47 live`, real content blocks, a real cold/warm split. ADC liveness was
established **before** the run (token minted, exit status only, never captured)
so that a failure would have been unambiguously a code finding rather than a
stale credential.

Budget headroom, and the one number that is not comfortable:

| Budget | Limit | Observed | Used |
|---|---:|---:|---:|
| background refresh fan-out | 10 s | 5.312 s | **53%** |
| process start -> ready | 3 s | 25.1 ms (debug) / 7.2 ms (release) | <1% |
| initialize -> first response | 100 ms | 17.0 ms (debug) / 0.50 ms (release) | 17% / 0.5% |
| snapshot parse | 500 ms | 15.4 ms | 3% |

**The refresh is the only budget without an order of magnitude in hand**, and
it was measured on a warm desktop with good network; it is the one that breaks
first on a worse link. It is listed here as the least-headroom budget rather
than filed among the comfortable ones.

Leak scan across both runs' logs: **0**. One benign log line worth knowing:
test 6 emits `WARN gcp_auth::types: failed to refresh token, trying again...
ConnectError("dns error", ... JoinError::Cancelled)`. That signature is runtime
teardown -- the test measures first response and shuts down while the
background credential fetch is mid-DNS -- not a failure. An operator who starts
and immediately stops the binary will see it.

Profiles: the table above is the **debug** profile, because
`CARGO_BIN_EXE_*` follows the test profile and README's documented command
carries no `--release`. The release cross-check is in section 9.

## 17. P8 -- scoped strip: measured, declined

**Candidate.** `[profile.release] strip = "debuginfo"` plus
`[profile.release.build-override] strip = "none"`. The scoped form had never
been tried; `ae92467` tested only a *global* strip, which broke proc-macro
dylibs. Registered estimate: `__LINKEDIT` 3,080,192 B and 56,854 local symbols
implied **~2.5-2.8 MB reclaimable**, roughly 3x the LTO win.

**Probe result: the form works, the estimate does not.** The build succeeds in
1m48s, `ae92467`'s corruption does **not** reproduce in `build-override` form,
the binary runs 47/548, `nextest` is 188/188, and the `[profile.bench]`
value-match freeze correctly shielded the benches from the new key -- its
designed failure mode, observed working. But the win is **-646,112 B (-4.0%)**,
about **4x smaller** than the estimate.

**Mechanism.** Only the nlist array is reclaimed: 38,669 stabs x 16 B =
618,704 B predicted against ~655,360 B observed (page-granular). The string
table barely moves, because the debug map's `FUN` strings are **shared** with
the regular symbol table -- stripping the stabs does not free the names. The
estimate had implicitly counted those strings twice.

**Corroborated independently on the shipped binary**, rather than recorded on
report: `nm -ap` counts **exactly 24 `OSO`** entries (the constant section 12
derived from arithmetic), 39,910 stab lines, and **19,576 `FUN`** entries --
the population whose strings are shared, which is the mechanism above.

**Decision: DECLINED (user).** -4.0% does not pay for losing symbolicated
backtraces on an operator-facing binary that handles credentials. Recorded as
a user decision on measured numbers, with the ~4x overestimate named so the
estimate is not re-proposed from the same reasoning.

## 18. Ship state (`77acb16`)

| Fact | Value |
|---|---|
| Binary | **16,097,184 B**, sha256 `fe5e314f...` |
| Build | `env -u RUSTFLAGS cargo build --release`, into `<repo>/target` (61-char path) |
| Delta from `871251c` (16,096,912 B) | **+272 B** -- fix batch #2 |
| Same commit, `/Volumes/tmpfs/target-sa` (24-char path) | 16,096,384 B -- see section 12; the 800 B is path length, not a different build |

The canonical build command carries no `direnv` and no `RUSTFLAGS`; the
`Reproduce` block above is the authority, and section 12 is why a size row
without its target-dir path cannot be compared.

## 19. What the review lanes caught, and about whom

Two findings from the review cycle are recorded with their provenance,
because in both cases the provenance is the lesson.

**F-HDRVAL was an exposure *created by* the MEDIUM-3 remediation, and caught
by a *different* lane.** A token that fetches successfully but cannot be
rendered as an HTTP header left through `?`, bypassing the failure cooldown
entirely, so the credential chain was re-walked on every call for as long as
the source kept producing it. That path did not exist before the MEDIUM-3 fix;
the remediation introduced it, and the code-review lane that produced the
remediation did not see it. The separate security lane did. This is not a
routine catch to be filed with the others: **a fix lane cannot review its own
remediation**, and the only reason this was caught is that two independent
lanes looked at the same code with different questions.

**The `googleapis.com` route invariant holds by construction in shipped
binaries, and is checked at test time -- never at runtime.** `debug_assert!` is
elided under `--release`, so no shipped binary evaluates it. What guarantees
the property in production is that `registry::ENDPOINTS` is a compile-time
constant containing only `*.googleapis.com` hosts; the assertion and its
`#[cfg(debug_assertions)]` guard test exist to catch a *future edit* to that
table during development. Any ledger or review text describing this as a
runtime check is wrong, and would overstate what ships.
