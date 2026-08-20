#!/usr/bin/env bash
#
# bench-startup.sh -- process start to `tools/list` answered, over stdio,
# against the RELEASE binary. N serial runs; reports min / median / p95 in ms.
#
# Modes (the banner says which one ran and what, if anything, was stubbed):
#
#   default          the real environment: real ADC, real Service Usage call,
#                    real background fan-out. This is the number a user feels.
#   --offline        the non-network portion in isolation, with no change to
#                    src/: credentials come from a throwaway service-account
#                    key whose token_uri is a local stub (GOOGLE_APPLICATION_
#                    CREDENTIALS), and every outbound HTTPS request is pointed
#                    through HTTPS_PROXY at a closed loopback port so it fails
#                    in microseconds instead of crossing the network. What is
#                    left is exactly what the real path does between those
#                    calls: provider discovery, a loopback token exchange,
#                    snapshot parse, catalog assembly, stdio handshake.
#   --print-catalog  `print-catalog` wall time (process start to exit). No MCP
#                    session at all: a network-free proxy for snapshot read +
#                    parse + render, kept because the optimization plan's
#                    baseline (section 0) quotes it.
#
# Rules enforced here because the numbers are compared across commits:
#   * runs are strictly serial; nothing else should be benchmarking meanwhile;
#   * RUSTFLAGS must be unset, so the binary under test is whatever
#     `[profile.release]` says and nothing ambient;
#   * the binary's identity (path, size, sha256, mtime) and the toolchain are
#     printed with every result, so a number is never separated from what
#     produced it. `cargo bench` re-links target/release/<bin> from the bench
#     profile; run `env -u RUSTFLAGS cargo build --release` again before
#     measuring, and compare the printed sha256 against BASELINE.md.
#
# Requires bash >= 5 (EPOCHREALTIME, named coprocs). --offline additionally
# needs python3 (token stub), openssl (throwaway RSA key) and jq (key JSON).

set -euo pipefail

if (( BASH_VERSINFO[0] < 5 )); then
    echo "bench-startup: bash >= 5 is required (EPOCHREALTIME, coproc); this is $BASH_VERSION" >&2
    echo "bench-startup: on macOS try: /opt/homebrew/bin/bash $0 ..." >&2
    exit 2
fi

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

RUNS=20
BIN="$ROOT/target/release/mcp-google-service"
PROJECT=""
MODE=real
SLEEP=1
TIMEOUT=60
KEEP_LOGS=0
STRICT=0

usage() {
    cat <<EOF
usage: $(basename "$0") [--runs N] [--bin PATH] [--project ID] [--sleep SECS]
                        [--timeout SECS] [--keep-logs] [--offline | --print-catalog]

  --runs N        serial iterations (default $RUNS)
  --bin PATH      release binary to measure (default $BIN)
  --project ID    quota project passed as --project (real mode only; otherwise
                  the binary resolves it from GOOGLE_MCP_QUOTA_PROJECT,
                  GOOGLE_CLOUD_PROJECT, or the ADC file)
  --sleep SECS    pause between runs, excluded from timing (default $SLEEP)
  --timeout SECS  per-run limit waiting for a response (default $TIMEOUT)
  --keep-logs     keep each run's stderr under the temp dir printed at the end
  --strict        pass --strict-startup to the server (credentials and
                  enablement resolved before serving, the pre-P2 path)
  --offline       stub credentials + dead proxy; see the header comment
  --print-catalog time the \`print-catalog\` subcommand instead of an MCP session
EOF
}

while (( $# )); do
    case $1 in
        --runs) RUNS=$2; shift 2 ;;
        --bin) BIN=$2; shift 2 ;;
        --project) PROJECT=$2; shift 2 ;;
        --sleep) SLEEP=$2; shift 2 ;;
        --timeout) TIMEOUT=$2; shift 2 ;;
        --keep-logs) KEEP_LOGS=1; shift ;;
        --strict) STRICT=1; shift ;;
        --offline) MODE=offline; shift ;;
        --print-catalog) MODE=print-catalog; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "bench-startup: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

die() { echo "bench-startup: $*" >&2; exit 1; }

[[ $RUNS =~ ^[1-9][0-9]*$ ]] || die "--runs must be a positive integer (got '$RUNS')"
[[ -x $BIN ]] || die "release binary not found or not executable: $BIN
build it first (no RUSTFLAGS):  env -u RUSTFLAGS cargo build --release"

# ---------------------------------------------------------------------------
# Flag regime: the binary under test must come from the profile alone.
# ---------------------------------------------------------------------------
if [[ -n ${RUSTFLAGS:-} ]]; then
    die "RUSTFLAGS is set ('$RUSTFLAGS'); benchmarks never run under RUSTFLAGS.
Unset it for the build AND for this run:  env -u RUSTFLAGS $0 $*"
fi
[[ -z ${CARGO_BUILD_RUSTFLAGS:-} ]] || die "CARGO_BUILD_RUSTFLAGS is set; unset it, benchmarks never run under ambient rustc flags"
[[ -z ${CARGO_ENCODED_RUSTFLAGS:-} ]] || die "CARGO_ENCODED_RUSTFLAGS is set; unset it, benchmarks never run under ambient rustc flags"

# ---------------------------------------------------------------------------
# Temp space, cleanup.
# ---------------------------------------------------------------------------
WORK=$(mktemp -d "${TMPDIR:-/tmp}/bench-startup.XXXXXX")
STUB_PID=""
CHILD_PID=""
cleanup() {
    [[ -n $CHILD_PID ]] && kill "$CHILD_PID" 2>/dev/null || true
    [[ -n $STUB_PID ]] && kill "$STUB_PID" 2>/dev/null || true
    if (( KEEP_LOGS )); then
        echo "bench-startup: logs kept under $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Mode setup.
# ---------------------------------------------------------------------------
STUBBED="none (real ADC, real Service Usage call, real background fan-out)"
case $MODE in
    real)
        if [[ -z $PROJECT && -z ${GOOGLE_MCP_QUOTA_PROJECT:-} && -z ${GOOGLE_CLOUD_PROJECT:-} ]]; then
            echo "bench-startup: note: no --project / GOOGLE_MCP_QUOTA_PROJECT / GOOGLE_CLOUD_PROJECT; the binary will need quota_project_id in the ADC file" >&2
        fi
        ;;
    offline)
        for tool in python3 openssl jq; do
            command -v "$tool" >/dev/null || die "--offline needs $tool on PATH"
        done
        # Throwaway key: generated per run, never written anywhere but $WORK,
        # and useful for nothing except satisfying gcp_auth's key parser.
        openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
            -out "$WORK/key.pem" 2>/dev/null \
            || die "openssl could not generate the throwaway RSA key"

        # Token stub: answers every POST with a fake bearer token. gcp_auth
        # retries a refused token endpoint five times with 50..400 ms backoff,
        # so a dead endpoint would add ~750 ms of sleep to the run; a stub
        # that answers is what keeps the loopback exchange at a few ms.
        python3 - "$WORK/stub.port" <<'PY' >"$WORK/stub.log" 2>&1 &
import http.server, json, socketserver, sys

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        body = json.dumps({"access_token": "bench-offline-token",
                           "expires_in": 3600, "token_type": "Bearer"}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass

server = socketserver.TCPServer(("127.0.0.1", 0), Handler)
with open(sys.argv[1], "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
PY
        STUB_PID=$!
        for _ in $(seq 1 50); do
            [[ -s $WORK/stub.port ]] && break
            sleep 0.1
        done
        [[ -s $WORK/stub.port ]] || die "token stub did not start; see $WORK/stub.log"
        STUB_PORT=$(<"$WORK/stub.port")

        jq -n --rawfile key "$WORK/key.pem" --arg uri "http://127.0.0.1:$STUB_PORT/token" '{
            type: "service_account",
            project_id: "bench-offline",
            private_key_id: "bench-offline",
            private_key: $key,
            client_email: "bench-offline@bench-offline.iam.gserviceaccount.com",
            client_id: "0",
            token_uri: $uri
        }' >"$WORK/service-account.json"

        export GOOGLE_APPLICATION_CREDENTIALS="$WORK/service-account.json"
        export GOOGLE_MCP_QUOTA_PROJECT="bench-offline"
        # Every https:// request the shared reqwest client makes (Service Usage
        # pruning, the 47-host refresh fan-out) goes to a proxy at a closed
        # loopback port and fails with ECONNREFUSED in microseconds. gcp_auth
        # uses its own client with no proxy support, so the token stub is
        # still reached directly.
        export HTTPS_PROXY="http://127.0.0.1:1" https_proxy="http://127.0.0.1:1"
        export ALL_PROXY="http://127.0.0.1:1" all_proxy="http://127.0.0.1:1"
        unset NO_PROXY no_proxy
        PROJECT=""
        STUBBED="GOOGLE_APPLICATION_CREDENTIALS -> throwaway service account, token_uri -> local stub on 127.0.0.1:$STUB_PORT; HTTPS_PROXY/ALL_PROXY -> http://127.0.0.1:1 (closed port: Service Usage prune and background fan-out fail instantly); quota project bench-offline"
        ;;
    print-catalog)
        STUBBED="n/a: no MCP session, no credentials; \`print-catalog\` reads $ROOT/data/catalog-snapshot.json from disk (cwd = repo root) and renders the table"
        ;;
esac

# ---------------------------------------------------------------------------
# Provenance banner.
# ---------------------------------------------------------------------------
# `wc -c` and `date -r FILE` behave the same under BSD and GNU userlands;
# `stat` does not (`-f` means format on one and filesystem on the other).
BIN_SIZE=$(wc -c <"$BIN" | tr -d ' ')
BIN_MTIME=$(date -r "$BIN" '+%Y-%m-%dT%H:%M:%S%z')
BIN_SHA=$(shasum -a 256 "$BIN" | cut -d' ' -f1)
TOOLCHAIN=$(cd "$ROOT" && rustc --version 2>/dev/null || echo "rustc: not on PATH")
CPU=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -p)

echo "bench-startup: mode=$MODE runs=$RUNS sleep=${SLEEP}s timeout=${TIMEOUT}s strict-startup=$( (( STRICT )) && echo yes || echo no )"
echo "  binary:    $BIN"
echo "  size:      $BIN_SIZE bytes  sha256=$BIN_SHA  mtime=$BIN_MTIME"
echo "  toolchain: $TOOLCHAIN  RUSTFLAGS=<unset>"
echo "  machine:   $(uname -srm); $CPU"
echo "  stubbed:   $STUBBED"
[[ -n ${RUST_LOG:-} ]] && echo "  RUST_LOG:  $RUST_LOG (inherited; affects how much the binary logs)"
[[ -n $PROJECT ]] && echo "  project:   $PROJECT (--project)"
echo "  measures:  $( [[ $MODE == print-catalog ]] && echo 'process start -> print-catalog exit' || echo 'process start -> tools/list response line read (initialize -> initialized -> tools/list, one client)' )"
echo

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"bench-startup","version":"0.1.0"}}}'
INITIALIZED_NOTE='{"jsonrpc":"2.0","method":"notifications/initialized"}'
LIST_REQ='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'

# Does this response line carry JSON-RPC id $2? serde_json emits `"id":N`;
# the optional space tolerates a pretty-printer.
has_id() {
    local line=$1 id=$2 re
    re="\"id\": ?${id}[,}]"
    [[ $line =~ $re ]]
}

# Read lines from fd $1 until one carries id $2; echoes the line. Returns 1 on
# timeout/EOF.
read_until_id() {
    local fd=$1 id=$2 line
    while IFS= read -r -t "$TIMEOUT" line <&"$fd"; do
        if has_id "$line" "$id"; then
            printf '%s\n' "$line"
            return 0
        fi
    done
    return 1
}

# Wait up to $2 seconds for pid $1 to exit; escalate TERM then KILL.
reap() {
    local pid=$1 budget=$2 waited=0
    while kill -0 "$pid" 2>/dev/null && (( waited < budget * 10 )); do
        sleep 0.1
        waited=$((waited + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        sleep 0.5
        kill -9 "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}

fail_run() {
    local n=$1 log=$2 why=$3
    echo >&2
    echo "bench-startup: run $n FAILED: $why" >&2
    echo "bench-startup: last lines of the server's stderr ($log):" >&2
    tail -n 20 "$log" >&2 || true
    KEEP_LOGS=1
    exit 1
}

# One MCP session; prints elapsed milliseconds (start -> tools/list answered).
run_mcp_once() {
    local n=$1 log="$WORK/run-$1.stderr" line t0 t1 in out pid
    local -a cmd=("$BIN")
    [[ -n $PROJECT ]] && cmd+=(--project "$PROJECT")
    (( STRICT )) && cmd+=(--strict-startup)

    t0=$EPOCHREALTIME
    coproc SRV { exec "${cmd[@]}" 2>"$log"; }
    in=${SRV[1]}; out=${SRV[0]}; pid=$SRV_PID
    CHILD_PID=$pid

    printf '%s\n' "$INIT_REQ" >&"$in" || fail_run "$n" "$log" "server closed stdin before initialize"
    line=$(read_until_id "$out" 1) || fail_run "$n" "$log" "no initialize response within ${TIMEOUT}s (or the server exited)"
    [[ $line == *'"result"'* ]] || fail_run "$n" "$log" "initialize was answered with an error: $line"
    printf '%s\n%s\n' "$INITIALIZED_NOTE" "$LIST_REQ" >&"$in" || fail_run "$n" "$log" "server closed stdin before tools/list"
    line=$(read_until_id "$out" 2) || fail_run "$n" "$log" "no tools/list response within ${TIMEOUT}s (or the server exited)"
    t1=$EPOCHREALTIME
    [[ $line == *'"tools"'* ]] || fail_run "$n" "$log" "tools/list was answered with an error: $line"

    # Closing stdin is the client going away; the server shuts down on EOF.
    exec {in}>&- 2>/dev/null || true
    reap "$pid" 10
    CHILD_PID=""
    awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", (b - a) * 1000 }'
}

# One `print-catalog` process; prints elapsed milliseconds (start -> exit).
run_print_catalog_once() {
    local n=$1 log="$WORK/run-$1.stderr" t0 t1 rc=0
    t0=$EPOCHREALTIME
    (cd "$ROOT" && "$BIN" print-catalog >/dev/null 2>"$log") || rc=$?
    t1=$EPOCHREALTIME
    (( rc == 0 )) || fail_run "$n" "$log" "print-catalog exited with status $rc"
    awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", (b - a) * 1000 }'
}

# ---------------------------------------------------------------------------
# Serial runs.
# ---------------------------------------------------------------------------
declare -a SAMPLES=()
for (( i = 1; i <= RUNS; i++ )); do
    if [[ $MODE == print-catalog ]]; then
        ms=$(run_print_catalog_once "$i")
    else
        ms=$(run_mcp_once "$i")
    fi
    SAMPLES+=("$ms")
    printf '  run %2d/%d: %9s ms\n' "$i" "$RUNS" "$ms"
    if (( i < RUNS )) && [[ $SLEEP != 0 ]]; then
        sleep "$SLEEP"
    fi
done

# ---------------------------------------------------------------------------
# Statistics: min / median / p95 (nearest rank) / max / mean.
# ---------------------------------------------------------------------------
SORTED=$(printf '%s\n' "${SAMPLES[@]}" | sort -n)
STATS=$(printf '%s\n' "$SORTED" | awk '
    { v[NR] = $1; sum += $1 }
    END {
        n = NR
        min = v[1]; max = v[n]
        median = (n % 2) ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2
        r = int(n * 0.95); if (r < n * 0.95) r++; if (r < 1) r = 1
        p95 = v[r]
        printf "%.2f %.2f %.2f %.2f %.2f", min, median, p95, max, sum / n
    }')
read -r MIN MEDIAN P95 MAX MEAN <<<"$STATS"

echo
echo "per-run ms (sorted): $(printf '%s ' $SORTED)"
echo "mode=$MODE runs=$RUNS  min=${MIN} ms  median=${MEDIAN} ms  p95=${P95} ms  max=${MAX} ms  mean=${MEAN} ms"
echo "summary: {\"mode\":\"$MODE\",\"runs\":$RUNS,\"min_ms\":$MIN,\"median_ms\":$MEDIAN,\"p95_ms\":$P95,\"max_ms\":$MAX,\"mean_ms\":$MEAN,\"binary_sha256\":\"$BIN_SHA\",\"binary_bytes\":$BIN_SIZE}"
