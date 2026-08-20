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
| P3 | `26b0cd3` | see section 9 | | | | |
| P4 | | | | | | |
| P5 | | | | | | |
| P6 | | | | | | |
| P7 | | | | | | |

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
