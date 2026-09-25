//! Stdio handshake tier: the shipped binary, driven with raw JSON-RPC lines.
//!
//! The other tiers reach the server through an rmcp client, which always
//! opens with `initialize`. Claude Code does not: with protocol negotiation
//! on, it probes with `server/discover` first and, when the probe is refused,
//! falls back to `initialize` on the same process. Only a harness that writes
//! the lines itself can send that order, so this one does.
//!
//! Hermetic like the integration tier. `GOOGLE_APPLICATION_CREDENTIALS` names
//! a file that does not exist, so credential discovery fails at its first
//! step without reading a developer's ADC or running `gcloud`; `CLOUDSDK_CONFIG`
//! names a directory that does not exist in case `gcloud` is ever run. Both proxy
//! variables name a closed loopback port, so neither the catalog refresh that
//! starts after `initialize` nor a metadata-server probe reaches anything.

use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    task::JoinHandle,
};

/// Quota project passed on the command line. Never a real project.
const TEST_PROJECT: &str = "test-project";

/// How long to wait for one reply. Generous because the first reply comes
/// only after a debug build has loaded the embedded catalog.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server gets to exit once its stdin is closed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The four meta-tools of the two-tier surface, in the order they are listed.
const TWO_TIER_TOOLS: [&str; 4] = ["list_services", "search_tools", "describe_tools", "call"];

/// The `_meta` a 2026-07-28 client attaches to each request.
fn modern_meta(protocol_version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": protocol_version,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "stdio-handshake-test", "version": "0" },
    })
}

/// The probe Claude Code 2.1.282 sends before `initialize`, id included.
fn discover_probe() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "server-discover-probe-1",
        "method": "server/discover",
        "params": { "_meta": modern_meta("2026-07-28") },
    })
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "stdio-handshake-test", "version": "0" },
        },
    })
}

fn initialized() -> Value {
    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
}

/// `tools/list` as a legacy session sends it: no `_meta`.
fn tools_list() -> Value {
    json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} })
}

/// `tools/list` carrying the per-request `_meta` a legacy session may omit.
fn tools_list_with_meta() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": { "_meta": modern_meta("2025-11-25") },
    })
}

/// The compiled server, spawned with stdio piped and no way to reach Google.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    /// Drains stderr for the whole run, so a chatty log can never fill the
    /// pipe and stall the server, and so a failure can quote it.
    stderr: JoinHandle<String>,
}

impl Server {
    fn spawn() -> Self {
        // Never created: credential discovery must fail on the missing file,
        // and `gcloud` must find no configuration should it ever be asked.
        let absent =
            std::env::temp_dir().join(format!("mcp-stdio-handshake-{}-absent", std::process::id()));
        let absent_credentials = absent.join("credentials.json");

        // A closed port for both schemes: `https_proxy` alone would leave the
        // plain-HTTP metadata server reachable.
        const CLOSED_PROXY: &str = "http://127.0.0.1:1";

        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-google-service"))
            .args(["serve", "--project", TEST_PROJECT, "--only", "bigquery"])
            .env("GOOGLE_APPLICATION_CREDENTIALS", &absent_credentials)
            .env("CLOUDSDK_CONFIG", &absent)
            .env("HTTPS_PROXY", CLOSED_PROXY)
            .env("https_proxy", CLOSED_PROXY)
            .env("HTTP_PROXY", CLOSED_PROXY)
            .env("http_proxy", CLOSED_PROXY)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the built binary must be spawnable");

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped")).lines();
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        });
        Self { child, stdin, stdout, stderr }
    }

    async fn write(&mut self, message: &Value) -> std::io::Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await
    }

    async fn read(&mut self) -> Result<Value, String> {
        let line = match tokio::time::timeout(REPLY_TIMEOUT, self.stdout.next_line()).await {
            Err(_elapsed) => return Err(format!("no reply within {REPLY_TIMEOUT:?}")),
            Ok(Err(error)) => return Err(format!("reading stdout failed: {error}")),
            Ok(Ok(None)) => return Err("the server closed stdout".to_owned()),
            Ok(Ok(Some(line))) => line,
        };
        serde_json::from_str(&line).map_err(|error| format!("reply `{line}` is not JSON: {error}"))
    }

    /// Close stdin, let the server exit, and return everything it logged.
    async fn finish(mut self) -> String {
        drop(self.stdin);
        if tokio::time::timeout(EXIT_TIMEOUT, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
        self.stderr.await.unwrap_or_else(|error| format!("<stderr drain panicked: {error}>"))
    }
}

/// The server's reply to each request of a session, in request order.
struct Transcript {
    replies: Vec<Value>,
    stderr: String,
}

impl Transcript {
    fn replies<const N: usize>(&self) -> &[Value; N] {
        self.replies.as_slice().try_into().unwrap_or_else(|_| {
            panic!(
                "expected {N} replies, got {}: {:#?}\nserver stderr:\n{}",
                self.replies.len(),
                self.replies,
                self.stderr
            )
        })
    }
}

/// Send `messages` in order to a fresh server, waiting for the reply to each
/// request before sending the next, as Claude Code does.
async fn replay(messages: &[Value]) -> Transcript {
    let mut server = Server::spawn();
    let mut replies = Vec::new();
    let mut failure = None;
    for message in messages {
        if let Err(error) = server.write(message).await {
            failure = Some(format!("writing {message} failed: {error}"));
            break;
        }
        let Some(id) = message.get("id") else { continue };
        match server.read().await {
            Ok(reply) if reply.get("id") == Some(id) => replies.push(reply),
            Ok(reply) => {
                failure = Some(format!("{message} was answered by {reply}, which has another id"));
                break;
            }
            Err(reason) => {
                failure = Some(format!("{message} got no reply: {reason}"));
                break;
            }
        }
    }
    let stderr = server.finish().await;
    if let Some(failure) = failure {
        panic!("{failure}\nreplies so far: {replies:#?}\nserver stderr:\n{stderr}");
    }
    Transcript { replies, stderr }
}

fn assert_initialized(reply: &Value, stderr: &str) {
    assert_eq!(
        reply.pointer("/result/protocolVersion"),
        Some(&json!("2025-11-25")),
        "`initialize` must settle on 2025-11-25; reply: {reply}\nserver stderr:\n{stderr}"
    );
}

fn assert_two_tier_tools(reply: &Value, stderr: &str) {
    let names: Option<Vec<&str>> = reply
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .map(|tools| tools.iter().filter_map(|tool| tool["name"].as_str()).collect());
    assert_eq!(
        names.as_deref(),
        Some(TWO_TIER_TOOLS.as_slice()),
        "`tools/list` must return the four two-tier tools; reply: {reply}\nserver stderr:\n{stderr}"
    );
}

/// The probe must be refused the way a legacy-only server refuses it --
/// `-32601 Method not found` under the probe's own id -- because that is the
/// answer a dual-era client falls back to `initialize` on.
fn assert_discover_refused_as_legacy(reply: &Value, stderr: &str) {
    assert_eq!(
        reply.get("id"),
        Some(&json!("server-discover-probe-1")),
        "the refusal must carry the probe's id; reply: {reply}\nserver stderr:\n{stderr}"
    );
    assert_eq!(
        reply.pointer("/error/code"),
        Some(&json!(-32601)),
        "`server/discover` must be refused with -32601 Method not found; reply: {reply}\n\
         server stderr:\n{stderr}"
    );
}

/// Scenario A: what Claude Code 2.1.282 sends with
/// `tengu_mcp_protocol_negotiation_stdio` on. Before the fix the refused probe
/// left rmcp demanding `_meta` on every later request, so this `tools/list`
/// failed with `-32602 request _meta is missing ...` and the client saw no
/// tools at all.
#[tokio::test]
async fn a_refused_discover_probe_leaves_a_usable_legacy_session() {
    let transcript = replay(&[discover_probe(), initialize(), initialized(), tools_list()]).await;
    let [discover, initialize, tools] = transcript.replies();

    assert_initialized(initialize, &transcript.stderr);
    assert_two_tier_tools(tools, &transcript.stderr);
    assert_discover_refused_as_legacy(discover, &transcript.stderr);
}

/// Scenario B: a client that never probes, which is every client before the
/// 2026-07-28 revision.
#[tokio::test]
async fn a_legacy_client_that_never_probes_lists_the_tools() {
    let transcript = replay(&[initialize(), initialized(), tools_list()]).await;
    let [initialize, tools] = transcript.replies();

    assert_initialized(initialize, &transcript.stderr);
    assert_two_tier_tools(tools, &transcript.stderr);
}

/// Scenario C: the probe, then a fallback session whose requests carry the
/// per-request `_meta` anyway.
#[tokio::test]
async fn a_fallback_session_that_sends_meta_anyway_lists_the_tools() {
    let transcript =
        replay(&[discover_probe(), initialize(), initialized(), tools_list_with_meta()]).await;
    let [discover, initialize, tools] = transcript.replies();

    assert_initialized(initialize, &transcript.stderr);
    assert_two_tier_tools(tools, &transcript.stderr);
    assert_discover_refused_as_legacy(discover, &transcript.stderr);
}
