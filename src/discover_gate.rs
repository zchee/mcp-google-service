//! Refuse `server/discover` before the session opens, the way a legacy-only
//! server would, so a probing client falls back to the legacy lifecycle rmcp
//! then serves correctly.
//!
//! # Why this exists
//!
//! Claude Code probes a stdio server with `server/discover` (revision
//! `2026-07-28`) before sending `initialize`, and falls back to `initialize`
//! on the same process when the probe is refused with anything other than a
//! modern-lifecycle error. This server does not offer `2026-07-28` (see
//! `GoogleMcpServer::supported_protocol_versions`), so the probe is refused,
//! and rmcp 3.4.1 then mishandles the fallback: `serve_server_with_ct_inner`
//! (`rmcp-3.4.1/src/service/server.rs:595-628`) marks the peer as requiring
//! per-request `_meta` the moment the first request is not `initialize`, and
//! never clears that mark. The `initialize` that follows is accepted, but the
//! legacy `tools/list` after it is refused with `-32602 request _meta is
//! missing ...`, and the client sees a connected server with no tools.
//!
//! The 2026-07-28 revision ("Backward Compatibility" under basic/lifecycle)
//! says a server that accepts `initialize` serves that stdio process as a
//! legacy session. Refusing the probe here, before rmcp sees it, keeps rmcp
//! in its pre-`initialize` state, so the fallback lands on the path that
//! already works.
//!
//! # When to delete this module
//!
//! rmcp upstream fixes the same defect in modelcontextprotocol/rust-sdk#1269
//! ("fix: preserve legacy fallback after rejected discovery"): a server whose
//! supported versions are all older than `2026-07-28` refuses discovery with
//! `-32601`, and a refused discovery no longer commits the connection to the
//! modern lifecycle. Once `Cargo.lock` pins an rmcp release that includes it,
//! pass `stdio()` to `serve` directly again and delete this module. The
//! `rmcp_still_mishandles_the_fallback_after_a_refused_probe` test fails on
//! such a release, which is the signal to do so.

use std::borrow::Cow;

use rmcp::{
    RoleServer,
    model::{
        ClientJsonRpcMessage, ClientRequest, DiscoverRequestMethod, ErrorData, ProtocolVersion,
        ServerJsonRpcMessage,
    },
    transport::Transport,
};

/// A [`Transport`] that answers `server/discover` itself until the session
/// opens.
///
/// Every other message, and every message after the request that opens the
/// session, is passed to rmcp untouched. Sending and closing are delegated
/// as they are.
pub struct DiscoverGate<T> {
    inner: T,
    /// Whether a discover probe is refused at all. False when the server
    /// offers a `2026-07-28`-or-newer revision, because then a probe is a
    /// legitimate opener that rmcp must see.
    refuses_probe: bool,
    /// Set once the request that opens rmcp's session has been passed
    /// through; the gate is inert after.
    ///
    /// That is the first request other than `ping`, not only `initialize`:
    /// rmcp also accepts an inline-lifecycle opener carrying full `_meta`,
    /// and once either has settled the lifecycle rmcp owns `server/discover`.
    /// It also keeps this gate's `receive` inside rmcp's sequential handshake
    /// loop, where it is awaited to completion; in the session loop that
    /// follows `receive` is polled under `select!` and could be dropped
    /// between reading a probe and writing its refusal.
    handshake_done: bool,
}

impl<T> DiscoverGate<T> {
    /// Wrap `inner` for a server that offers exactly `supported`.
    ///
    /// The gate refuses probes only when every offered revision predates
    /// `2026-07-28`, which is the same test rust-sdk#1269 applies. A server
    /// that offers a modern revision gets its probes delivered, so adding
    /// `2026-07-28` to the offered set turns this gate off without a code
    /// change here.
    pub fn new(inner: T, supported: &[ProtocolVersion]) -> Self {
        let refuses_probe =
            supported.iter().all(|version| *version < ProtocolVersion::V_2026_07_28);
        Self { inner, refuses_probe, handshake_done: false }
    }

    /// Whether the next probe would be refused.
    #[cfg(test)]
    fn refuses_probe(&self) -> bool {
        self.refuses_probe && !self.handshake_done
    }
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for DiscoverGate<T> {
    type Error = T::Error;

    fn name() -> Cow<'static, str> {
        format!("DiscoverGate<{}>", T::name()).into()
    }

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        loop {
            let message = self.inner.receive().await?;
            if self.handshake_done {
                return Some(message);
            }
            match message {
                ClientJsonRpcMessage::Request(request)
                    if self.refuses_probe
                        && matches!(request.request, ClientRequest::DiscoverRequest(_)) =>
                {
                    tracing::debug!(
                        id = ?request.id,
                        "refusing `server/discover` before `initialize` so the client falls \
                         back to the legacy lifecycle"
                    );
                    let refusal = ServerJsonRpcMessage::error(
                        ErrorData::method_not_found::<DiscoverRequestMethod>(),
                        Some(request.id),
                    );
                    if let Err(error) = self.inner.send(refusal).await {
                        tracing::warn!(
                            error = %error,
                            "could not answer the `server/discover` probe; closing the session"
                        );
                        return None;
                    }
                }
                ClientJsonRpcMessage::Request(request) => {
                    if !matches!(request.request, ClientRequest::PingRequest(_)) {
                        self.handshake_done = true;
                    }
                    return Some(ClientJsonRpcMessage::Request(request));
                }
                other => return Some(other),
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[cfg(test)]
#[path = "discover_gate_tests.rs"]
mod tests;
