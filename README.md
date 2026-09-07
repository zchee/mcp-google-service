# mcp-google-service

A single MCP server that aggregates Google Cloud's remote MCP endpoints behind
one process, one authentication model, and one namespaced tool surface.

Google publishes MCP endpoints per service at
`https://{service}.googleapis.com/mcp` (with ten exceptions, listed under
[The endpoint registry](#the-endpoint-registry)). Registering them
individually in an MCP client runs into three problems, all measured against
the live endpoints (2026-08-19, re-measured 2026-08-31 after the registry grew
to 65):

- **756 tools across 65 endpoints**, which is far more than a model can be
  offered at once.
- **46 tool-name collisions** between services (several services publish a tool
  called `list_services`, for example).
- **Hourly credential expiry.** Application Default Credentials access tokens
  expire after roughly an hour, so a client config holding a static
  `Authorization` header stops working and cannot refresh itself.

This server addresses all three: it namespaces every tool as
`{service}__{tool}`, prunes to the APIs actually enabled on your project,
exposes four meta-tools instead of hundreds of real ones, and mints a fresh
token for each upstream call.

## Design, and the evidence behind it

Each decision below follows from a property that was verified against the live
endpoints rather than assumed.

| Design point | Evidence |
|---|---|
| One auth shape for every endpoint: `Authorization: Bearer <ADC token, scope cloud-platform>` plus `x-goog-user-project: <quota project>` | Verified against `run`, `bigquery`, `logging`, and `developerknowledge`; no per-endpoint branching was needed. |
| Tool discovery needs no credentials | `initialize` and `tools/list` answered unauthenticated on all 65 endpoints (47 probed 2026-08-19, 18 more 2026-08-31), which is what makes the bundled snapshot and the credential-free startup path possible. |
| Prune to enabled APIs | Service Usage reports roughly 5 of the registered APIs enabled on a typical project, so pruning removes the `SERVICE_DISABLED` failure mode before the model ever sees the tool. |
| Namespace with `{service}__{tool}` | Eliminates all 46 collisions by construction, and the prefix is what dispatch routes on. |
| Serve from a snapshot, refresh in the background | A full 65-endpoint fan-out takes 7.7-7.9s (measured 2026-08-31; concurrency 16 keeps it flat as the registry grows). Doing it on the startup path would delay the first tool response by that much, so startup serves the bundled snapshot and swaps in live data when it arrives. |

Because the two-tier surface exposes a fixed set of four tools, swapping the
catalog underneath the server needs no `listChanged` notification.

## Requirements

- Rust 1.97.1 (edition 2024, MSRV 1.88), pinned exactly by `rust-toolchain.toml`.
- The `gcloud` CLI, for creating credentials.
- A Google Cloud project to use as the quota project.

## Build

```sh
cargo build --release
```

The binary lands at `target/release/mcp-google-service`. It embeds a catalog
snapshot at compile time, so it runs standalone without the repository.

## Authentication setup

The server never handles a password or key of its own; it reads Application
Default Credentials and forwards a short-lived access token.

1. **Create Application Default Credentials.**

   ```sh
   gcloud auth application-default login
   ```

2. **Set a quota project.** Every Cloud endpoint requires
   `x-goog-user-project`; without it, calls fail with a 403 telling you a quota
   project is required.

   ```sh
   gcloud auth application-default set-quota-project PROJECT_ID
   ```

   The server resolves the quota project in this order, and fails with a
   message naming all four mechanisms if none yields a value:

   1. the `--project` flag
   2. `GOOGLE_MCP_QUOTA_PROJECT`
   3. `GOOGLE_CLOUD_PROJECT`
   4. `quota_project_id` in the ADC file (`GOOGLE_APPLICATION_CREDENTIALS`, or
      `~/.config/gcloud/application_default_credentials.json`)

3. **Grant IAM roles.** Calling any tool requires the `mcp.tools.call`
   permission, which ships in `roles/mcp.toolUser`:

   ```sh
   gcloud projects add-iam-policy-binding PROJECT_ID \
     --member="user:you@example.com" --role="roles/mcp.toolUser"
   ```

   That role governs only the MCP call itself. Each tool additionally requires
   the product's own role, so reading Cloud Run services also needs something
   like `roles/run.viewer`. Consult the product documentation for the exact
   role a given tool needs.

4. **Enable the APIs you intend to use.** Only enabled APIs are exposed:

   ```sh
   gcloud services enable run.googleapis.com --project=PROJECT_ID
   ```

## Registering with Claude Code

```sh
claude mcp add gcp -- /absolute/path/to/target/release/mcp-google-service --project PROJECT_ID
```

Use an absolute path: Claude Code does not resolve the binary against your
shell's `PATH` in every launch context. The server speaks MCP on stdout and
writes logs to stderr, so log output never corrupts the protocol stream.

## The endpoint registry

`src/registry.rs` pins 65 endpoints. The table lists them all; the
subsections after it explain the ten that do not follow the derived naming
rule, how a query spells an id, and why no regional host is pinned.

### Supported endpoints

All 65, in the order [`list_services`](#the-tool-surface) and
[`print-catalog`](#command-line-interface) report them. The tool prefix is the
service id (`{prefix}__{tool}`); the Service Usage name is
what `gcloud services enable` takes and what pruning matches on; the tool
counts are those of the snapshot pinned 2026-08-31. A test holds this table
to the registry and the embedded snapshot, so a registry change or a
snapshot refresh that is not reflected here fails the build.

Every row is reachable once its API is enabled, with one exception: `design`
needs `--only design`, per [the note
below](#the-design-endpoint-and-enablement-pruning).

| Tool prefix | MCP endpoint | Service Usage API | Tools |
|---|---|---|---|
| `agentregistry` | `agentregistry.googleapis.com/mcp` | `agentregistry.googleapis.com` | 20 |
| `alloydb` | `alloydb.googleapis.com/mcp` | `alloydb.googleapis.com` | 17 |
| `androidmanagement` | `androidmanagement.googleapis.com/mcp` | `androidmanagement.googleapis.com` | 9 |
| `apihub` | `apihub.googleapis.com/mcp` | `apihub.googleapis.com` | 46 |
| `backupdr` | `backupdr.googleapis.com/mcp` | `backupdr.googleapis.com` | 23 |
| `bigquery` | `bigquery.googleapis.com/mcp` | `bigquery.googleapis.com` | 6 |
| `bigquerydatatransfer` | `bigquerydatatransfer.googleapis.com/mcp` | `bigquerydatatransfer.googleapis.com` | 13 |
| `bigquerymigration` | `bigquerymigration.googleapis.com/mcp` | `bigquerymigration.googleapis.com` | 10 |
| `bigtableadmin` | `bigtableadmin.googleapis.com/mcp` | `bigtableadmin.googleapis.com` | 14 |
| `ces` | `ces.us.rep.googleapis.com/mcp` | `ces.googleapis.com` | 60 |
| `cloudasset` | `cloudasset.googleapis.com/mcp` | `cloudasset.googleapis.com` | 4 |
| `cloudbilling` | `cloudbilling.googleapis.com/mcp` | `cloudbilling.googleapis.com` | 29 |
| `cloudcli` | `cloudcli.googleapis.com/mcp` | `cloudcli.googleapis.com` | 2 |
| `clouderrorreporting` | `clouderrorreporting.googleapis.com/mcp` | `clouderrorreporting.googleapis.com` | 1 |
| `cloudlocationfinder` | `cloudlocationfinder.googleapis.com/mcp` | `cloudlocationfinder.googleapis.com` | 2 |
| `cloudproductregistry` | `cloudproductregistry.googleapis.com/mcp` | `cloudproductregistry.googleapis.com` | 7 |
| `cloudquotas` | `cloudquotas.googleapis.com/mcp` | `cloudquotas.googleapis.com` | 5 |
| `cloudresourcemanager` | `cloudresourcemanager.googleapis.com/mcp` | `cloudresourcemanager.googleapis.com` | 1 |
| `cloudsupport` | `cloudsupport.googleapis.com/mcp` | `cloudsupport.googleapis.com` | 6 |
| `cloudtrace` | `cloudtrace.googleapis.com/mcp` | `cloudtrace.googleapis.com` | 2 |
| `compute` | `compute.googleapis.com/mcp` | `compute.googleapis.com` | 29 |
| `container` | `container.googleapis.com/mcp` | `container.googleapis.com` | 23 |
| `databasecenter` | `databasecenter.googleapis.com/mcp` | `databasecenter.googleapis.com` | 6 |
| `databaseinsights` | `databaseinsights.googleapis.com/mcp` | `databaseinsights.googleapis.com` | 7 |
| `dataform` | `dataform.googleapis.com/mcp` | `dataform.googleapis.com` | 21 |
| `datalineage` | `datalineage.googleapis.com/mcp` | `datalineage.googleapis.com` | 1 |
| `datamigration` | `datamigration.googleapis.com/mcp` | `datamigration.googleapis.com` | 8 |
| `dataplex` | `dataplex.googleapis.com/mcp` | `dataplex.googleapis.com` | 3 |
| `dataproc` | `dataproc.googleapis.com/mcp` | `dataproc.googleapis.com` | 16 |
| `datastream` | `datastream.googleapis.com/mcp` | `datastream.googleapis.com` | 10 |
| `design` | `design.googleapis.com/mcp` | `design.googleapis.com` | 5 |
| `developerknowledge` | `developerknowledge.googleapis.com/mcp` | `developerknowledge.googleapis.com` | 3 |
| `discoveryengine` | `discoveryengine.googleapis.com/mcp` | `discoveryengine.googleapis.com` | 3 |
| `file` | `file.googleapis.com/mcp` | `file.googleapis.com` | 8 |
| `firestore` | `firestore.googleapis.com/mcp` | `firestore.googleapis.com` | 25 |
| `geminicloudassist` | `geminicloudassist.googleapis.com/mcp` | `geminicloudassist.googleapis.com` | 5 |
| `homedevelopers` | `homedevelopers.googleapis.com/mcp` | `homedevelopers.googleapis.com` | 1 |
| `logging` | `logging.googleapis.com/mcp` | `logging.googleapis.com` | 6 |
| `mapscodeassist` | `mapscodeassist.googleapis.com/mcp` | `mapscodeassist.googleapis.com` | 2 |
| `mapstools` | `mapstools.googleapis.com/mcp` | `mapstools.googleapis.com` | 5 |
| `memorystore` | `memorystore.googleapis.com/mcp` | `memorystore.googleapis.com` | 14 |
| `monitoring` | `monitoring.googleapis.com/mcp` | `monitoring.googleapis.com` | 9 |
| `netapp` | `netapp.googleapis.com/mcp` | `netapp.googleapis.com` | 14 |
| `networkmanagement` | `networkmanagement.googleapis.com/mcp` | `networkmanagement.googleapis.com` | 4 |
| `oracledatabase` | `oracledatabase.googleapis.com/mcp` | `oracledatabase.googleapis.com` | 39 |
| `paydeveloper` | `paydeveloper.googleapis.com/mcp` | `paydeveloper.googleapis.com` | 13 |
| `policytroubleshooter` | `policytroubleshooter.googleapis.com/mcp` | `policytroubleshooter.googleapis.com` | 2 |
| `pubsub` | `pubsub.googleapis.com/mcp` | `pubsub.googleapis.com` | 15 |
| `recommender` | `recommender.googleapis.com/mcp` | `recommender.googleapis.com` | 4 |
| `redis` | `redis.googleapis.com/mcp` | `redis.googleapis.com` | 25 |
| `run` | `run.googleapis.com/mcp` | `run.googleapis.com` | 5 |
| `saasservicemgmt` | `saasservicemgmt.googleapis.com/mcp` | `saasservicemgmt.googleapis.com` | 35 |
| `servicehealth` | `servicehealth.googleapis.com/mcp` | `servicehealth.googleapis.com` | 2 |
| `spanner` | `spanner.googleapis.com/mcp` | `spanner.googleapis.com` | 15 |
| `sqladmin` | `sqladmin.googleapis.com/mcp` | `sqladmin.googleapis.com` | 15 |
| `stitch` | `stitch.googleapis.com/mcp` | `stitch.googleapis.com` | 15 |
| `vertex-endpoints` | `aiplatform.googleapis.com/mcp/endpoints` | `aiplatform.googleapis.com` | 6 |
| `vertex-evaluation` | `aiplatform.googleapis.com/mcp/evaluation` | `aiplatform.googleapis.com` | 1 |
| `vertex-generate` | `aiplatform.googleapis.com/mcp/generate` | `aiplatform.googleapis.com` | 3 |
| `vertex-models` | `aiplatform.googleapis.com/mcp/models` | `aiplatform.googleapis.com` | 11 |
| `vertex-predict` | `aiplatform.googleapis.com/mcp/predict` | `aiplatform.googleapis.com` | 2 |
| `vertex-prompts` | `aiplatform.googleapis.com/mcp/prompts` | `aiplatform.googleapis.com` | 11 |
| `vertex-retrieval` | `aiplatform.googleapis.com/mcp/retrieval` | `aiplatform.googleapis.com` | 4 |
| `vertex-tuning` | `aiplatform.googleapis.com/mcp/tuning` | `aiplatform.googleapis.com` | 4 |
| `vtx-notebook` | `aiplatform.googleapis.com/mcp/notebook` | `aiplatform.googleapis.com` | 24 |

### The derived rule and its ten exceptions

Fifty-five of them follow one derived rule, host `{service}.googleapis.com`,
path `/mcp`, Service Usage API name equal to the host, and are written as bare
ids. Ten do not, and each exception records a fact about Google's layout
rather than a choice made here:

| Tool prefix | Served from | Why it is manual |
|---|---|---|
| `vertex-endpoints`, `vertex-evaluation`, `vertex-generate`, `vertex-models`, `vertex-predict`, `vertex-prompts`, `vertex-retrieval`, `vertex-tuning` | `aiplatform.googleapis.com/mcp/{suite}` | Vertex AI publishes nine tool suites as paths on one shared host rather than one host per service, and Service Usage knows them all as `aiplatform.googleapis.com`. The registry pins the **global** host. The documentation also lists 46 regional hosts and 2 continental `rep` ones (`us`, `eu`); the global host answers discovery (probed 2026-08-31) and tool metadata does not vary by host, so the catalog needs no region. |
| `vtx-notebook` | `aiplatform.googleapis.com/mcp/notebook` | The ninth Vertex suite. Its Colab Enterprise tool names run to 49 characters, and `vertex-notebook__` in front of one makes 66, over the 64-character limit MCP clients enforce on tool names; 13 id characters is the budget, so this one suite breaks the `vertex-` pattern. |
| `ces` | `ces.us.rep.googleapis.com/mcp` | Customer Experience Agent Studio is served only from continental `rep` hosts; there is no `ces.googleapis.com/mcp`. The US host is pinned, as the documentation page shows. The EU variant, `ces.eu.rep.googleapis.com/mcp`, answers discovery too (probed 2026-09-01) and is deliberately not a second entry; see [Regional and EU routing](#regional-and-eu-routing). |

### Spelling a service id in a query

A `-` in a service id is a word boundary, so `vertex generate` names the
`vertex-generate` suite the way `cloud asset` names `cloudasset`, and `vertex`
alone reaches every `vertex-*` suite. The notebook suite's short id means
`vertex notebook` does not name it: reach it with `notebook` or `colab`, each
of which leads with the suite's own tools, or with `vtx notebook`.

### Regional and EU routing

This is a follow-up, not a registry entry. Discovery is host-invariant, but
every `call` goes to the pinned host: Vertex calls go to
the global endpoint and Agent Studio calls to the US one. A deployment that
must keep its data in a region, or in the EU, has no way to say so yet. That
is a serving-configuration concern (a per-service host override), tracked as
the data-residency follow-up; adding EU or regional rows to the registry would
only duplicate tool names without giving the operator the choice.

### The design endpoint and enablement pruning

`design.googleapis.com` answers discovery and its five tools are in the
snapshot, but Service Usage knows no API by that name: it is absent both from
a project's enabled services and from the services available to be enabled
(checked 2026-09-08 against a real project). Pruning matches an endpoint's
Service Usage name exactly, so the entry is dropped on every project and its
tools never reach the model. Pass `--only design` to skip pruning; discovery
and dispatch themselves work.

The name that does gate it, if there is one, is unknown.
`designcenter.googleapis.com` is a different MCP server serving different
tools, not this one under another name. Recovering the right name needs an
authenticated call whose `SERVICE_DISABLED` error names the API, so the entry
keeps its derived name rather than a guess.

## The tool surface

By default the server exposes four meta-tools rather than 756 real ones.
Schemas load on demand, so the model gets exact argument shapes without paying
for them up front.

| Tool | Purpose |
|---|---|
| `list_services` | Exposed services, each with tool count, Service Usage API name, whether its tools came from a live fetch or the snapshot, and its readiness (`pending`, `ready`, `unverified`, `failed`; see [Startup](#startup-what-is-resolved-when)). A `startup` block carries the credential and enablement state with the failure text, if any. |
| `search_tools` | Rank tools by keyword across all exposed services. Takes `query`, optional `service`, optional `limit` (default 20). |
| `describe_tools` | Full input and output JSON schemas for named tools. Takes `names`. |
| `call` | Invoke a namespaced tool and return its result unchanged. Takes `name` and `arguments`. |

The intended sequence is `list_services` or `search_tools` to find a tool,
`describe_tools` to learn its arguments, then `call`.

### Usage transcript

```jsonc
// 1. What is available?
list_services {}
{
  "services": [
    { "service_id": "run", "api_name": "run.googleapis.com", "tool_count": 12, "source": "live" },
    { "service_id": "logging", "api_name": "logging.googleapis.com", "tool_count": 8, "source": "snapshot" }
  ],
  "service_count": 2,
  "tool_count": 20
}

// 2. Find something by keyword.
search_tools { "query": "list cloud run services", "limit": 3 }
{
  "query": "list cloud run services",
  "match_count": 1,
  "matches": [
    { "name": "run__list_services", "service_id": "run", "description": "List Cloud Run services in a region." }
  ]
}

// 3. Learn the exact arguments before calling.
describe_tools { "names": ["run__list_services"] }
{
  "tools": [
    {
      "name": "run__list_services",
      "service_id": "run",
      "upstream_name": "list_services",
      "source": "live",
      "description": "List Cloud Run services in a region.",
      "input_schema": {
        "type": "object",
        "properties": { "project": { "type": "string" }, "region": { "type": "string" } },
        "required": ["project", "region"]
      },
      "output_schema": null
    }
  ],
  "unknown": []
}

// 4. Invoke it. Credentials and the quota project are attached automatically.
call { "name": "run__list_services", "arguments": { "project": "PROJECT_ID", "region": "us-central1" } }
```

A `source` of `snapshot` means that service's tools came from the bundled
catalog because its live fetch failed, or because the fetched tool names could
not be namespaced within the 64-character limit MCP clients enforce (one
upstream rename degrades that one service, never the whole catalog). The data
may be stale; the accompanying `WARN` log names the service, the host and
path, and the cause.

### Flat mode

`--expose flat` registers every pruned, namespaced tool with its real upstream
schema instead of the four meta-tools. This suits clients that handle large
tool lists well, or that struggle with a generic `call` tool. Dispatch is
identical in both modes.

**The flat tool list is fixed at startup.** Two-tier mode always answers from
the freshest catalog available, because its own four tool names never change
and the background refresh can be swapped in underneath them. Flat mode cannot
do that: the client is handed concrete tool names at `initialize`, and this
server sends no `listChanged` notification, so a refresh that moved the list
would leave the client offering tools the server no longer has and hiding tools
it does. Flat therefore keeps serving the catalog it started with; restart the
server to pick up a changed upstream tool set.

For the same reason flat mode resolves credentials and the enabled-API list
*before* serving (it implies `--strict-startup`), so its time to the first tool
response stays at the network-bound figure (~1.8 s on the reference machine)
while two-tier's is ~23 ms. The alternative -- serving every configured
service unpruned and letting a disabled API fail at call time with the
`SERVICE_DISABLED` remediation -- would make flat start as fast, but it would
hand the client a tool list containing tools that cannot be used, and the
remediation only helps after a failed call; listing unusable tools degrades
the model's view of the world, so it was rejected.

## Command-line interface

```
mcp-google-service [OPTIONS] [COMMAND]
```

| Option | Meaning |
|---|---|
| `--project <PROJECT>` | Quota project for `x-goog-user-project` and Service Usage pruning. Also read from `GOOGLE_MCP_QUOTA_PROJECT`. Accepts either spelling Google accepts: a project **id** (6-30 characters, `[a-z][a-z0-9-]{4,28}[a-z0-9]`) or a project **number** (1-20 digits). |
| `--only <IDS>` | Expose only these service ids, comma-separated. Skips enablement pruning. |
| `--exclude <IDS>` | Never expose these service ids, comma-separated. Applied last, so it wins over `--only`. |
| `--expose <MODE>` | `two-tier` (default) or `flat`. |
| `--snapshot <PATH>` | Serve tool metadata from this snapshot file instead of the embedded one. Unreadable or invalid paths are fatal. |
| `--strict-startup` | Acquire credentials and the enabled-API list **before** serving, and exit if credentials cannot be acquired. By default both are resolved in the background after `initialize` is answered (see [Startup](#startup-what-is-resolved-when)); this flag restores the fail-fast behaviour. Implied by `--expose flat`. |

These are serving options. They belong either before a subcommand
(`mcp-google-service --project p`, which runs `serve` by default) or on `serve`
itself (`mcp-google-service serve --project p`); `snapshot` and `print-catalog`
neither accept nor advertise them.

`--only` takes precedence over enablement: if it is non-empty, those services
are exposed and Service Usage is not consulted for the decision. `--exclude` is
then applied to whatever survived, so a service named on both flags is not
exposed. The deny-list is the half that wins, because excluding a service is a
statement that it must not be reachable.

| Subcommand | Purpose |
|---|---|
| `serve` | Run the stdio MCP server. This is the default when no subcommand is given. |
| `snapshot [--out PATH] [--allow-partial]` | Fetch every registered endpoint live and emit the catalog as JSON. Exits non-zero if any endpoint could not be reached, unless `--allow-partial` is given. |
| `print-catalog` | Print the bundled snapshot's per-service tool counts. Needs no credentials. |

```sh
mcp-google-service print-catalog
```

```
generated_at: 2026-08-31T18:56:55Z

SERVICE               TOOLS  SOURCE
agentregistry            20  live
alloydb                  17  live
...
65 services             756
```

### A note on `>` under zsh

`snapshot` writes to stdout by default. Prefer `--out` over a shell redirect:

```sh
mcp-google-service snapshot --out data/catalog-snapshot.json
```

With zsh's `noclobber` option set, `> existing-file` fails with
`file exists` and the command's output is lost. If you do redirect, use `>|`:

```sh
mcp-google-service snapshot >| data/catalog-snapshot.json
```

## Error messages

Upstream failures are classified into messages that name the fixing command,
rather than passed through as raw HTTP errors. Failures reach the caller as an
MCP error *result* rather than a protocol error, so the model can read the
remediation and act on it.

The messages below describe an upstream call, so on the dispatch path each one
arrives prefixed with the host it came from: `call to run.googleapis.com
failed: upstream returned 403: ...`. Upstream text that is passed through
rather than classified is stripped of control characters and truncated at 2KiB
first, so a remote party cannot rewrite an operator's terminal or flood a
model's context through an error body.

| Message begins | Meaning | Fix |
|---|---|---|
| `upstream returned 401: the request is missing required authentication credentials` | No usable credential was found. | `gcloud auth application-default login` |
| `upstream returned 401: API keys are not supported by this API` | An API key was sent; Cloud endpoints accept only OAuth2. | Use ADC; remove the API key. |
| `upstream returned 403: the request requires a quota project` | No `x-goog-user-project` accompanied the call. | `--project`, `GOOGLE_MCP_QUOTA_PROJECT`, or `gcloud auth application-default set-quota-project` |
| `upstream returned 403: {api} is disabled in project {project}` | The API is not enabled. | The message contains the exact command: `gcloud services enable {api} --project={project}` |
| `upstream returned 403: permission denied` | The caller lacks IAM permission. | Grant `roles/mcp.toolUser` plus the product's own role. |
| `unknown service {id}` | The tool prefix matches no exposed service. | Run `list_services`; the message lists the valid prefixes. |
| `{name} is not a namespaced tool name` | The name lacks a `{service}__{tool}` prefix. | Use the namespaced name, for example `run__list_services`. |
| `upstream returned {status}: {body}` | Anything else, passed through as text. | Read the body. |

### Errors this server raises itself

These come from the server rather than from Google. The first two are fatal at
startup; the rest are reported and survivable.

| Message begins | Meaning | Fix |
|---|---|---|
| `could not attach Google credentials: failed to acquire Google credentials via ADC` (on `call`) / `failed to acquire Google credentials via ADC` (at startup with `--strict-startup`) | No usable Application Default Credentials were found, or the credential source refused. By default this is reported by `list_services` (`readiness: failed`, the reason in `startup.credentials_error`) and returned by every `call` until a later attempt succeeds; with `--strict-startup` it ends the process before anything is served. A failed (or timed-out) fetch is not retried for 30 s: calls inside that window return the same text at once, suffixed `the credential source is not retried for another Ns`, instead of each paying the credential chain's own retries. | `gcloud auth application-default login`, then retry the call; no restart is needed unless `--strict-startup` was used. |
| `the resolved quota project is neither a valid Google Cloud project id nor a project number` | The resolved value matches neither grammar. It may have come from `GOOGLE_MCP_QUOTA_PROJECT`, `GOOGLE_CLOUD_PROJECT` or the ADC file, not just `--project`, and it is never echoed back. | Pass an id (`my-project`) or a number (`123456789012`). Check the environment and `quota_project_id` in the ADC file. |
| `catalog snapshot {path} could not be read` / `is not a valid catalog snapshot` | An explicit `--snapshot <PATH>` is missing or unparseable. Never falls back. | Fix the path, or drop the flag to use the embedded snapshot. |
| `failed to deserialize ApplicationCredentials: missing field private_key` | The ADC file is an **impersonated** credential (`type: impersonated_service_account`), which `gcp_auth` 0.12.7 cannot use. The message is misleading: nothing is missing, the credential shape is simply unsupported. **Do not create a service-account key to satisfy it.** | Re-run `gcloud auth application-default login` *without* `--impersonate-service-account`. |
| `WARN ... failed to refresh token, trying again ... dns error ... JoinError::Cancelled` seen on immediate shutdown | Not a failure. The background credential fetch was cancelled mid-DNS by process teardown; `Interrupted` plus `JoinError::Cancelled` is the signature. Expected when the server is started and stopped at once. | None. |
| `acquiring a Google access token timed out after 30s` | The credential source (ADC, metadata server, `gcloud`) did not answer. | Check `gcloud auth application-default print-access-token`, or the metadata server's reachability. |
| `Service Usage returned a pagination token it had already served` / `Service Usage listing did not terminate within 50 pages` | The enabled-API listing stopped making progress and was abandoned. Pruning degrades: the configured selection is exposed unpruned, with a `WARN`. | None required. If it persists, `--only` pins the services to expose without consulting Service Usage. |
| `` `call` requires `arguments` to be a JSON object `` | `call` was given `arguments` as an array, string, number or boolean. | Read the schema with `describe_tools` and pass an object. Omitting `arguments` (or passing `null`) is valid for a tool that takes none. |

## The catalog snapshot

`data/catalog-snapshot.json` is a committed, evidence-dated capture of all 65
endpoints (756 tools, pinned 2026-08-31). It is embedded into the binary at
compile time and serves three purposes: it makes startup fast, it lets a
binary run without its repository, and it provides per-host fallback when a
live fetch fails.

**Serving reads the embedded copy only.** The file in the working tree is not
consulted, and neither is any `data/catalog-snapshot.json` in whatever
directory the client happened to launch the server from. Tool descriptions and
schemas are instructions a model acts on, so supplying them is an explicit
decision: pass `--snapshot <PATH>` to serve a different file, and an unreadable
or invalid path there fails the process rather than quietly falling back.
(`print-catalog` does read the working tree; reporting on the repository it is
run in is its purpose.)

At startup the server serves that snapshot immediately and runs **one** live
refresh in the background, swapping the result in when it lands. The refresh is
deliberately one-shot rather than periodic: a long-running session's tool
surface stays stable, and a client that wants a fresh catalog restarts the
server. Until the refresh lands, every service reports `source: snapshot`, so
`list_services` never overstates freshness.

### Startup: what is resolved when

Nothing that needs the network sits on the path to the first tool response.
`serve` parses the snapshot, answers `initialize` and `tools/list`, and only
then -- in the background, in this order -- acquires credentials, asks Service
Usage which APIs are enabled (unless `--only` pinned the set), narrows the
exposed services to the enabled ones, and runs the live refresh. Measured on
the reference machine this takes process start to `tools/list` from ~1.8 s
(one Service Usage listing alone was ~1.5 s of it) to the non-network floor
(`BASELINE.md`).

Until those steps land, `list_services` shows every configured service with
`readiness: pending`; a `call` made meanwhile simply waits for credentials and
then goes upstream, where a disabled API answers with the classified
`SERVICE_DISABLED` remediation. When they land, the readiness flips to `ready`
(or `unverified` if Service Usage could not be consulted, in which case the
configured selection stays exposed unpruned) and the `startup` block records
the outcome. A credential failure is not swallowed: `list_services` reports
`readiness: failed` with the reason, every `call` returns it, and the next
call retries discovery, so `gcloud auth application-default login` takes
effect without a restart.

`--strict-startup` restores the earlier behaviour for operators who would
rather the process exit than serve with broken credentials: credentials and
enablement are resolved before serving and a credential failure is fatal.
`--expose flat` implies it, because the flat tool list is fixed at
`initialize` and so must be final before anything is served.

### Regenerating and reviewing drift

```sh
cargo build --release
./target/release/mcp-google-service snapshot --out data/catalog-snapshot.json
git diff --stat data/catalog-snapshot.json
```

`snapshot` exits non-zero if any registered endpoint failed to answer, and
writes nothing: a snapshot missing services becomes a binary that silently
cannot offer their tools. Pass `--allow-partial` to record a partial capture
deliberately.

The snapshot is pretty-printed rather than compact (11.6MB versus 7.4MB) so
that `git diff` produces reviewable line-level changes.

On startup, drift between the snapshot and the live catalog is logged as a
diff naming tools added, removed, and schema-changed.

**Caveat: `cloudcli` description variance.** Drift detection deliberately
compares tool *name sets* and schema digests, not description text. The
`cloudcli` endpoint serves per-replica description variants (`cloudcli__run_bq_command`
flip-flops between wordings depending on which replica answers), so comparing
descriptions would report drift forever. Tool names and counts are stable
on every observed run. When reviewing a regenerated snapshot, expect
description-only churn on `cloudcli` and disregard it; treat name or schema
changes as real.

## Testing

```sh
cargo nextest run
```

The default run is hermetic: no credentials, no Google network. Integration
tests run real in-process MCP servers as upstreams rather than canned fakes,
so the genuine protocol is exercised.

### Live tests

The live tier talks to real Google endpoints and is inert unless explicitly
enabled. It exercises read-only tools only.

```sh
MCP_GOOGLE_LIVE=1 GOOGLE_MCP_QUOTA_PROJECT=PROJECT_ID \
  cargo nextest run -E 'test(live_)'
```

| Variable | Purpose |
|---|---|
| `MCP_GOOGLE_LIVE=1` | Enables the tier. Without it the tests run, report that they are inert, and pass. |
| `GOOGLE_MCP_QUOTA_PROJECT` | The project to bill and prune against. There is no default; the tests refuse to guess one. |

Live tests also need working ADC. They assert published latency budgets:
process start to ready under 3s, initialize to first tool response under
100ms, snapshot parse under 500ms, and one background refresh fan-out under
10s.

Run them serially -- three of them are timing assertions, and concurrent
servers measure each other's contention:

```sh
MCP_GOOGLE_LIVE=1 GOOGLE_MCP_QUOTA_PROJECT=PROJECT_ID \
  cargo nextest run -E 'test(live_)' --test-threads=1
```

**The command above measures the debug binary.** The tests resolve the server
through `CARGO_BIN_EXE_*`, so they exercise whichever profile the test run
built. The budgets are loose enough that both profiles pass, so the difference
is easy to miss and easy to compare across by mistake: on the same commit,
initialize to first response measures ~4ms in debug and ~0.5ms in release, and
start to ready ~55ms against ~7ms. Add `--release` to measure the shipped
artifact, and never compare a number from one profile against a number from
the other.

## Manual end-to-end checklist

Automated tests do not cover the experience of driving the two-tier surface
from a real client. Run this once per release:

1. Build a release binary: `cargo build --release`.
2. Register it: `claude mcp add gcp-test -- "$(pwd)/target/release/mcp-google-service" --project PROJECT_ID`.
3. Confirm the server appears connected and lists exactly four tools.
4. Call `list_services`. Confirm the services shown match the APIs enabled on
   the project, and note which report `source: snapshot`.
5. Call `search_tools` with a plain-language query and confirm the ranking is
   plausible.
6. Call `describe_tools` on a hit and confirm a complete input schema comes
   back.
7. Call `call` with `run__list_services` and confirm a non-error result.
8. Call a tool belonging to a disabled API and confirm the error names the
   exact `gcloud services enable` command.
9. Remove the test registration: `claude mcp remove gcp-test`.

## Scope

Supported: Cloud endpoints at `https://{service}.googleapis.com/mcp` over
stdio, with ADC credentials, plus the manual entries in
[The endpoint registry](#the-endpoint-registry): the nine Vertex AI suites at
`aiplatform.googleapis.com/mcp/{suite}` on the global host, and Customer
Experience Agent Studio at its US `rep` host.

Not supported in v1, each for a specific reason:

- Google Workspace `/mcp/v1` endpoints, which need consumer OAuth scopes that
  ADC is not known to satisfy.
- Choosing a regional Vertex host or the EU `rep` host for Agent Studio: the
  data-residency follow-up described in the registry section. Both answer
  discovery; neither is selectable for calls yet.
- `storage.googleapis.com/storage/mcp`, which was not probed.
- API-key authentication. Cloud endpoints reject it outright: "API keys are
  not supported by this API."
- An HTTP-facing server mode. stdio covers Claude Code.
- **Impersonated ADC** (`gcloud auth application-default login
  --impersonate-service-account=...`). The underlying `gcp_auth` 0.12.7 has no
  impersonation support, so the credential is rejected while being parsed. See
  the troubleshooting row below -- the error names a missing `private_key`,
  which is misleading for a credential that deliberately has no key.
- **Workload Identity Federation** (`external_account` ADC) is **unprobed**,
  like the regional hosts above: it was never exercised, so it is listed as
  untested rather than claimed to work or claimed to fail.
