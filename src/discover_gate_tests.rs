use std::{io, time::Duration};

use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        ClientJsonRpcMessage, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, RequestId, ServerCapabilities, ServerJsonRpcMessage, Tool,
    },
    service::{QuitReason, RequestContext},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::*;

/// The id Claude Code 2.1.282 gives its probe.
const PROBE_ID: &str = "server-discover-probe-1";

/// Time allowed for one reply from the in-process service loop.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Wire messages, parsed the way the stdio codec parses them
// ---------------------------------------------------------------------------

fn client_message(raw: Value) -> ClientJsonRpcMessage {
    serde_json::from_value(raw.clone())
        .unwrap_or_else(|error| panic!("{raw} is not a client message: {error}"))
}

fn modern_meta(protocol_version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": protocol_version,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "gate-test", "version": "0" },
    })
}

fn discover() -> ClientJsonRpcMessage {
    client_message(json!({
        "jsonrpc": "2.0",
        "id": PROBE_ID,
        "method": "server/discover",
        "params": { "_meta": modern_meta("2026-07-28") },
    }))
}

fn initialize() -> ClientJsonRpcMessage {
    client_message(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "gate-test", "version": "0" },
        },
    }))
}

fn initialized() -> ClientJsonRpcMessage {
    client_message(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
}

fn ping(id: u64) -> ClientJsonRpcMessage {
    client_message(json!({ "jsonrpc": "2.0", "id": id, "method": "ping" }))
}

fn tools_list(id: u64) -> ClientJsonRpcMessage {
    client_message(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {} }))
}

fn tools_list_with_meta(id: u64) -> ClientJsonRpcMessage {
    client_message(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": { "_meta": modern_meta("2025-11-25") },
    }))
}

fn method_of(message: &ClientJsonRpcMessage) -> String {
    let raw = serde_json::to_value(message).expect("client messages serialize");
    raw["method"].as_str().map(str::to_owned).unwrap_or_else(|| format!("<no method: {raw}>"))
}

fn to_wire(message: &ServerJsonRpcMessage) -> Value {
    serde_json::to_value(message).expect("server messages serialize")
}

// ---------------------------------------------------------------------------
// A transport over channels, with the test holding the client end
// ---------------------------------------------------------------------------

struct ChannelTransport {
    incoming: mpsc::UnboundedReceiver<ClientJsonRpcMessage>,
    outgoing: mpsc::UnboundedSender<ServerJsonRpcMessage>,
}

struct ClientEnd {
    to_server: mpsc::UnboundedSender<ClientJsonRpcMessage>,
    from_server: mpsc::UnboundedReceiver<ServerJsonRpcMessage>,
}

impl ClientEnd {
    fn send(&self, message: ClientJsonRpcMessage) {
        self.to_server.send(message).expect("the server end is still open");
    }

    /// The next message the server wrote, as wire JSON.
    async fn next(&mut self) -> Value {
        match tokio::time::timeout(REPLY_TIMEOUT, self.from_server.recv()).await {
            Ok(Some(message)) => to_wire(&message),
            Ok(None) => panic!("the server closed its side without answering"),
            Err(_elapsed) => panic!("no reply within {REPLY_TIMEOUT:?}"),
        }
    }

    /// Send a request and return the server's reply as wire JSON.
    async fn ask(&mut self, message: ClientJsonRpcMessage) -> Value {
        self.send(message);
        self.next().await
    }

    /// Everything the server has written so far, without waiting.
    fn drain(&mut self) -> Vec<Value> {
        let mut written = Vec::new();
        while let Ok(message) = self.from_server.try_recv() {
            written.push(to_wire(&message));
        }
        written
    }
}

fn channel_transport() -> (ChannelTransport, ClientEnd) {
    let (to_server, incoming) = mpsc::unbounded_channel();
    let (outgoing, from_server) = mpsc::unbounded_channel();
    (ChannelTransport { incoming, outgoing }, ClientEnd { to_server, from_server })
}

impl Transport<RoleServer> for ChannelTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let outgoing = self.outgoing.clone();
        async move {
            outgoing
                .send(item)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the client went away"))
        }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        self.incoming.recv().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.incoming.close();
        Ok(())
    }
}

/// The versions the shipped server offers (`GoogleMcpServer`): nothing at or
/// past `2026-07-28`.
const LEGACY_ONLY: &[ProtocolVersion] =
    &[ProtocolVersion::V_2025_11_25, ProtocolVersion::V_2025_06_18];

fn legacy_gate(inner: ChannelTransport) -> DiscoverGate<ChannelTransport> {
    DiscoverGate::new(inner, LEGACY_ONLY)
}

/// `gate.receive()` under a deadline, so a gate that swallows a message it
/// should have yielded fails the test instead of hanging it.
async fn received(gate: &mut DiscoverGate<ChannelTransport>) -> Option<ClientJsonRpcMessage> {
    tokio::time::timeout(REPLY_TIMEOUT, gate.receive())
        .await
        .unwrap_or_else(|_elapsed| panic!("the gate yielded nothing within {REPLY_TIMEOUT:?}"))
}

fn assert_method_not_found(reply: &Value) {
    assert_eq!(reply["id"], json!(PROBE_ID), "the refusal must carry the probe's id: {reply}");
    assert_eq!(reply["error"]["code"], json!(-32601), "expected Method not found: {reply}");
    assert_eq!(reply["error"]["message"], json!("server/discover"), "{reply}");
    assert!(reply.get("result").is_none(), "a refusal carries no result: {reply}");
}

// ---------------------------------------------------------------------------
// The gate on its own
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_probe_before_initialize_is_refused_and_never_yielded() {
    let (inner, mut client) = channel_transport();
    let mut gate = legacy_gate(inner);
    assert!(gate.refuses_probe());

    client.send(discover());
    client.send(initialize());

    let yielded = received(&mut gate).await.expect("initialize reaches the caller");
    assert_eq!(method_of(&yielded), "initialize", "the probe must be swallowed, not yielded");
    assert!(!gate.refuses_probe(), "the gate is inert once initialize has passed");

    let written = client.drain();
    assert_eq!(written.len(), 1, "exactly one refusal: {written:?}");
    assert_method_not_found(&written[0]);
}

#[tokio::test]
async fn every_probe_before_initialize_is_refused() {
    let (inner, mut client) = channel_transport();
    let mut gate = legacy_gate(inner);

    client.send(discover());
    client.send(discover());
    client.send(initialize());

    let yielded = received(&mut gate).await.expect("initialize reaches the caller");
    assert_eq!(method_of(&yielded), "initialize");

    let written = client.drain();
    assert_eq!(written.len(), 2, "one refusal per probe: {written:?}");
    for refusal in &written {
        assert_method_not_found(refusal);
    }
}

#[tokio::test]
async fn a_probe_after_initialize_is_passed_through() {
    let (inner, mut client) = channel_transport();
    let mut gate = legacy_gate(inner);

    client.send(initialize());
    client.send(discover());

    let first = received(&mut gate).await.expect("initialize");
    assert_eq!(method_of(&first), "initialize");
    let second = received(&mut gate).await.expect("discover");
    assert_eq!(method_of(&second), "server/discover", "rmcp owns discover after initialize");

    assert!(client.drain().is_empty(), "the gate must write nothing after initialize");
}

/// rmcp answers a pre-`initialize` ping itself and, under the inline
/// lifecycle, decides what to do with any other opener; the gate must not
/// pre-empt either. Notifications never get a reply at all.
#[tokio::test]
async fn everything_except_a_probe_is_passed_through_before_the_session_opens() {
    let (inner, mut client) = channel_transport();
    let mut gate = legacy_gate(inner);

    client.send(ping(10));
    client.send(initialized());
    client.send(ping(11));

    let mut methods = Vec::new();
    for _ in 0..3 {
        let yielded = received(&mut gate).await.expect("each message reaches the caller");
        methods.push(method_of(&yielded));
    }
    assert_eq!(methods, ["ping", "notifications/initialized", "ping"]);
    assert!(gate.refuses_probe(), "neither ping nor a notification opens the session");
    assert!(client.drain().is_empty(), "the gate must write nothing for pass-through traffic");

    client.send(discover());
    client.send(initialize());
    let yielded = received(&mut gate).await.expect("initialize reaches the caller");
    assert_eq!(method_of(&yielded), "initialize", "the probe after them is still refused");
    assert_eq!(client.drain().len(), 1);
}

/// rmcp accepts an opener other than `initialize` when it carries full
/// `_meta` (the inline lifecycle), and any other first request is rmcp's to
/// refuse. Either way the lifecycle is settled by then, so a later probe is
/// rmcp's, not the gate's.
#[tokio::test]
async fn any_opener_other_than_ping_disarms_the_gate() {
    for opener in [
        tools_list_with_meta(11),
        tools_list(11),
        client_message(json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "x-vendor/custom",
            "params": {},
        })),
    ] {
        let (inner, mut client) = channel_transport();
        let mut gate = legacy_gate(inner);
        let opener_method = method_of(&opener);

        client.send(opener);
        client.send(discover());

        let first = received(&mut gate).await.expect("the opener reaches the caller");
        assert_eq!(method_of(&first), opener_method);
        assert!(!gate.refuses_probe(), "`{opener_method}` opened the session");
        let second = received(&mut gate).await.expect("the probe reaches the caller");
        assert_eq!(method_of(&second), "server/discover", "after `{opener_method}`");
        assert!(client.drain().is_empty(), "the gate must write nothing after `{opener_method}`");
    }
}

/// The gate mirrors rust-sdk#1269's test, "every offered revision predates
/// 2026-07-28". Offer a modern revision and a probe is a legitimate opener
/// that rmcp must handle, so the gate steps aside.
#[tokio::test]
async fn the_gate_is_inert_when_a_modern_revision_is_offered() {
    let (inner, mut client) = channel_transport();
    let mut gate =
        DiscoverGate::new(inner, &[ProtocolVersion::V_2025_11_25, ProtocolVersion::V_2026_07_28]);
    assert!(!gate.refuses_probe());

    client.send(discover());

    let yielded = received(&mut gate).await.expect("the probe reaches the caller");
    assert_eq!(method_of(&yielded), "server/discover");
    assert!(client.drain().is_empty());
}

#[test]
fn an_empty_offer_still_refuses_the_probe() {
    let (inner, _client) = channel_transport();
    let gate = DiscoverGate::new(inner, &[]);
    assert!(gate.refuses_probe(), "`all` over nothing is true, and there is nothing to discover");
}

#[tokio::test]
async fn a_refusal_that_cannot_be_written_ends_the_session() {
    let (inner, client) = channel_transport();
    let mut gate = legacy_gate(inner);

    client.send(discover());
    // The client stops reading, so the refusal has nowhere to go.
    drop(client.from_server);

    assert!(received(&mut gate).await.is_none(), "an unanswerable probe must end the session");
}

#[tokio::test]
async fn end_of_input_is_passed_through() {
    let (inner, client) = channel_transport();
    let mut gate = legacy_gate(inner);
    drop(client);
    assert!(received(&mut gate).await.is_none());
}

#[tokio::test]
async fn send_and_close_are_delegated_to_the_inner_transport() {
    let (inner, mut client) = channel_transport();
    let mut gate = legacy_gate(inner);

    let outgoing = ServerJsonRpcMessage::error(
        ErrorData::internal_error("delegated", None),
        Some(RequestId::Number(7)),
    );
    gate.send(outgoing).await.expect("send is delegated");
    let written = client.drain();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["error"]["message"], json!("delegated"));

    gate.close().await.expect("close is delegated");
    assert!(
        client.to_server.send(discover()).is_err(),
        "the inner transport stopped accepting input, so the client cannot write to it"
    );
    assert!(received(&mut gate).await.is_none(), "the inner transport was closed");
}

#[test]
fn the_transport_name_says_what_it_wraps() {
    let name = <DiscoverGate<ChannelTransport> as Transport<RoleServer>>::name();
    assert!(name.starts_with("DiscoverGate<"), "{name}");
    assert!(name.contains("ChannelTransport"), "{name}");
}

// ---------------------------------------------------------------------------
// The gate under rmcp's real service loop
// ---------------------------------------------------------------------------

/// A server shaped like the shipped one where it matters here: offers no
/// revision at or past `2026-07-28`, and lists one tool.
#[derive(Clone)]
struct LegacyOnlyServer;

impl ServerHandler for LegacyOnlyServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(LEGACY_ONLY)
    }

    fn get_info(&self) -> InitializeResult {
        let mut info = InitializeResult::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tool = Tool::new("one_tool", "The one tool.", rmcp::model::JsonObject::new());
        Ok(ListToolsResult { tools: vec![tool], ..Default::default() })
    }
}

/// Run `LegacyOnlyServer` over `transport` on rmcp's own service loop.
///
/// The returned task resolves once the client end is dropped.
fn serve<T: Transport<RoleServer> + 'static>(transport: T) -> tokio::task::JoinHandle<QuitReason> {
    tokio::spawn(async move {
        let running = LegacyOnlyServer.serve(transport).await.expect("the handshake completes");
        running.waiting().await.expect("the service task does not panic")
    })
}

fn tool_names(reply: &Value) -> Option<Vec<&str>> {
    reply["result"]["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(|tool| tool["name"].as_str()).collect())
}

/// The defect this module exists for, pinned so that the module's removal
/// condition is checked by the build rather than remembered.
///
/// Without the gate, rmcp 3.4.1 refuses the probe with `-32022`, accepts the
/// `initialize` that follows, and then refuses the legacy `tools/list` with
/// `-32602`. When this test fails because that last reply succeeds, rmcp has
/// picked up rust-sdk#1269 (or equivalent): delete this module and hand
/// `stdio()` to `serve` directly.
#[tokio::test]
async fn rmcp_still_mishandles_the_fallback_after_a_refused_probe() {
    let (transport, mut client) = channel_transport();
    let server = serve(transport);

    let probe = client.ask(discover()).await;
    assert_eq!(
        probe["error"]["code"],
        json!(-32022),
        "bare rmcp 3.4.1 refuses the probe as an unsupported version. A `-32601` here \
         means rmcp now refuses discovery itself (rust-sdk#1269), so the DiscoverGate \
         has served its purpose: delete `discover_gate.rs` and pass `stdio()` to \
         `serve` directly. Reply was: {probe}"
    );

    let handshake = client.ask(initialize()).await;
    assert_eq!(handshake["result"]["protocolVersion"], json!("2025-11-25"), "{handshake}");
    client.send(initialized());

    let tools = client.ask(tools_list(2)).await;
    assert_eq!(
        tools["error"]["code"],
        json!(-32602),
        "rmcp no longer demands `_meta` after a refused probe, so the DiscoverGate \
         has served its purpose: delete `discover_gate.rs` and pass `stdio()` to \
         `serve` directly. Reply was: {tools}"
    );

    drop(client);
    assert!(matches!(server.await.expect("server task"), QuitReason::Closed));
}

/// Scenario A at the unit level: the probe is refused as
/// `-32601`, and the legacy session that follows lists tools.
#[tokio::test]
async fn the_gate_restores_the_legacy_fallback_under_rmcp() {
    let (transport, mut client) = channel_transport();
    let server = serve(legacy_gate(transport));

    let probe = client.ask(discover()).await;
    assert_method_not_found(&probe);

    let handshake = client.ask(initialize()).await;
    assert_eq!(handshake["result"]["protocolVersion"], json!("2025-11-25"), "{handshake}");
    client.send(initialized());

    let tools = client.ask(tools_list(2)).await;
    assert_eq!(tool_names(&tools).as_deref(), Some(["one_tool"].as_slice()), "{tools}");

    drop(client);
    assert!(matches!(server.await.expect("server task"), QuitReason::Closed));
}

/// Scenario B: a client that never probes.
#[tokio::test]
async fn a_client_that_never_probes_is_unaffected_by_the_gate() {
    let (transport, mut client) = channel_transport();
    let server = serve(legacy_gate(transport));

    let handshake = client.ask(initialize()).await;
    assert_eq!(handshake["result"]["protocolVersion"], json!("2025-11-25"), "{handshake}");
    client.send(initialized());

    let tools = client.ask(tools_list(2)).await;
    assert_eq!(tool_names(&tools).as_deref(), Some(["one_tool"].as_slice()), "{tools}");

    drop(client);
    assert!(matches!(server.await.expect("server task"), QuitReason::Closed));
}

/// Scenario C: the probe, then a fallback session that carries `_meta`
/// anyway.
#[tokio::test]
async fn a_fallback_session_carrying_meta_still_lists_tools_through_the_gate() {
    let (transport, mut client) = channel_transport();
    let server = serve(legacy_gate(transport));

    let probe = client.ask(discover()).await;
    assert_method_not_found(&probe);

    let handshake = client.ask(initialize()).await;
    assert_eq!(handshake["result"]["protocolVersion"], json!("2025-11-25"), "{handshake}");
    client.send(initialized());

    let tools = client.ask(tools_list_with_meta(2)).await;
    assert_eq!(tool_names(&tools).as_deref(), Some(["one_tool"].as_slice()), "{tools}");

    drop(client);
    assert!(matches!(server.await.expect("server task"), QuitReason::Closed));
}

/// rmcp answers a pre-`initialize` ping itself; the gate must leave that
/// path intact, probe or no probe.
#[tokio::test]
async fn a_ping_before_initialize_is_still_answered_by_rmcp() {
    let (transport, mut client) = channel_transport();
    let server = serve(legacy_gate(transport));

    let pong = client.ask(ping(0)).await;
    assert_eq!(pong["id"], json!(0), "{pong}");
    assert!(pong.get("error").is_none(), "{pong}");

    let probe = client.ask(discover()).await;
    assert_method_not_found(&probe);

    let handshake = client.ask(initialize()).await;
    assert_eq!(handshake["result"]["protocolVersion"], json!("2025-11-25"), "{handshake}");

    drop(client);
    assert!(matches!(server.await.expect("server task"), QuitReason::Closed));
}
