//! Unified multi-transport accept layer for lamco-rdp-server.
//!
//! See `~/lamco-admin/shared/strategy/TRANSPORT-LAYER-DECISION-2026-05-16.md`
//! and `docs/design/transport/TRANSPORT-PHASE-1-SDS-2026-05-16.md`
//! for the architectural rationale.
//!
//! Three-tier contract: `Transport → Handshake → Byte stream → IronRDP acceptor`.
//! Phase 1 ships only TCP; later phases add AF_VSOCK, Unix sockets, and
//! WebSocket+RDCleanPath without changing the public trait shape.
//!
//! Deployment abstraction (`AcceptDeployment` trait) added by Phase-1-retrofit
//! 2026-05-16 — encapsulates per-binary differences in listener construction,
//! handler wiring, and shutdown signaling so the dispatcher stays binary-agnostic.

pub mod config;
pub mod handler;
pub mod handshake_deadline;
pub mod listener;
pub mod proxy_auth;
pub mod socket_activation;
#[cfg(feature = "websocket")]
pub mod websocket;

use std::{
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

use anyhow::Result;
pub use config::TransportsConfig;
use futures::stream::{FuturesUnordered, StreamExt};
pub use handler::LamcoConnectionHandler;
use ironrdp_server::{PostConnectionAction, RdpServer};
pub use listener::{
    AcceptedConnection, AcceptorMode, AsyncRdpStream, Listener, PeerAddr, TransportError,
};
pub use proxy_auth::{AllowAllInsecure, DenyAll, ProxyAuthValidator, SharedSecretValidator};
pub use socket_activation::{ActivatedFds, ActivationError};
use tracing::{debug, error, info, warn};

/// Configures and runs an `AcceptDispatcher` for a specific binary's runtime
/// context (desktop, qemu, future vsock-only, future WebSocket gateway, etc).
///
/// Encapsulates the per-binary differences in: which transports to bind,
/// what state the per-connection handler needs (event sink, PAM validator,
/// portal-validity closure), and how to detect shutdown. The dispatcher
/// consumes this uniform interface and doesn't care which binary it's running
/// inside.
///
/// Implementors today: `crate::server::deployment::WlrDirectDeployment`, which
/// despite its name backs every desktop-sharing session strategy (Portal,
/// Mutter Direct, libei, wlr-direct alike) — not just the wlr-direct one.
/// Phase 2 adds `crate::qemu::deployment::QemuDeployment`.
#[async_trait::async_trait]
pub trait AcceptDeployment: Send {
    /// Diagnostic name surfaced in logs (e.g. `"desktop"`, `"qemu"`).
    fn name(&self) -> &'static str;

    /// Bind all listeners for this deployment. Called once at dispatcher
    /// startup. Encapsulates LISTEN_FDS handling, socket activation,
    /// transport-config source (TOML schema for wlr-direct, programmatic for
    /// qemu), and Cargo-feature gating.
    async fn build_listeners(&mut self) -> Result<Vec<Box<dyn Listener>>>;

    /// Called when a connection's serving server is chosen (per-transport
    /// security routing). Default: no-op (single-server deployments).
    ///
    /// Dual-server deployments retarget per-server wiring here — most
    /// importantly the display handler's event sender, which is a single
    /// `mpsc` command channel into the serving server (EGFX/cursor/rdpsnd
    /// commands go to whichever server actually serves; a stale sender would
    /// silently drop them into the idle server).
    fn on_server_routed(&mut self, _route: ServerRoute) {}

    /// Construct the per-connection handler with deployment-appropriate
    /// wiring (event sink, PAM validator, portal-validity closure). Called
    /// once.
    fn build_handler(&mut self) -> LamcoConnectionHandler;

    /// Future that resolves when this deployment's shutdown is signaled.
    /// Abstracts away the native channel type — `broadcast::Receiver` for
    /// wlr-direct, `watch::Receiver` for qemu, etc.
    fn shutdown_signal(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// Drives the unified accept loop. Consumes a deployment, borrows the
/// `RdpServer` for the loop's duration, and yields when shutdown fires or a
/// connection-handler hook returns `PostConnectionAction::Stop`.
///
/// Phase 1 (TCP only). Phase 2 adds Unix. Phases 3a / 3b add AF_VSOCK and
/// WebSocket+RDCleanPath — all without changing this entry point.
pub struct AcceptDispatcher;

/// Per-transport security routing (Hyper-V Enhanced Session).
///
/// `security_mode = "rdp"` keeps the single plain-RDP server and upstream
/// v1.4.5 behavior exactly (all connections unencrypted). Otherwise, when
/// the vsock transport is resolved, the desktop deployment builds a SECOND
/// server configured `with_no_security`: vmms terminates TLS and CredSSP on
/// the host and relays plaintext Standard RDP Security over the
/// hypervisor-isolated vsock, which no TLS-configured server can accept
/// ("received corrupt message ... InvalidContentType", upstream #52).
/// TCP/Unix/WSS connections keep the primary server's TLS/Hybrid security.
///
/// The pair is serial-use: exactly one server serves a connection at a time
/// (the dispatcher's accept loop is serial), so the desktop deployment
/// re-points the display handler's event sender at whichever server just
/// accepted — see `on_server_routed`.
#[derive(Debug, Clone, Copy)]
pub enum ServerRoute {
    /// Serve on the primary server (TLS/Hybrid per `security_mode`).
    /// All non-vsock transports.
    Primary,
    /// Serve on the plain-RDP server (Standard RDP Security, no TLS).
    /// vsock only, and only when a secondary server exists.
    Plain,
}

/// The one-or-two servers a deployment runs, with per-transport routing.
///
/// `plain` is `None` unless the vsock transport is active and
/// `security_mode != "rdp"` (in which case the primary IS the plain server
/// and there is nothing secondary to build).
pub struct ServerPair {
    primary: RdpServer,
    plain: Option<RdpServer>,
}

impl ServerPair {
    /// Single-server pair (upstream v1.4.5 shape).
    pub fn single(server: RdpServer) -> Self {
        Self {
            primary: server,
            plain: None,
        }
    }

    /// Two-server pair: `primary` (TLS/Hybrid) + `plain` (no security) for
    /// host-relayed vsock connections.
    pub fn dual(primary: RdpServer, plain: RdpServer) -> Self {
        Self {
            primary,
            plain: Some(plain),
        }
    }

    /// True when a secondary plain-RDP server exists (vsock active,
    /// security_mode != "rdp").
    pub fn has_plain(&self) -> bool {
        self.plain.is_some()
    }

    /// Decide which server serves a connection, by transport and acceptor
    /// mode. vsock routes to the plain server when one exists; everything
    /// else stays on the primary. A vsock connection with no plain server
    /// (security_mode = "rdp": primary already speaks Standard RDP Security)
    /// routes to the primary — identical wire behavior.
    pub fn route(&self, transport: &str, mode: AcceptorMode) -> ServerRoute {
        route_connection(transport, mode, self.has_plain())
    }

    /// Mutable access to the server a route selects — the dispatcher hands
    /// it to `run_connection` / `run_connection_with`.
    pub fn server_for(&mut self, route: ServerRoute) -> &mut RdpServer {
        match route {
            ServerRoute::Primary => &mut self.primary,
            ServerRoute::Plain => self
                .plain
                .as_mut()
                .expect("ServerRoute::Plain implies plain server exists"),
        }
    }

    /// Mutable access to the primary (upstream single-server call sites).
    pub fn primary_mut(&mut self) -> &mut RdpServer {
        &mut self.primary
    }

    /// The primary server's event-sender channel (inbound command channel —
    /// Quit/Disconnect/Egfx/Rdpsnd/AutoDetectRttRequest). The desktop
    /// deployment installs this into the display handler at startup and
    /// re-points it per route via `on_server_routed`.
    pub fn primary_event_sender(
        &self,
    ) -> &tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent> {
        self.primary.event_sender()
    }

    /// The plain server's event sender, when a plain server exists.
    pub fn plain_event_sender(
        &self,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>> {
        self.plain.as_ref().map(|s| s.event_sender())
    }

    /// The primary's ErrorInfo disconnect handle (client-visible graceful
    /// disconnect). `error_info_disconnect_handle` is `&self` on the
    /// RdpServer.
    pub fn primary_error_info_disconnect_handle(&self) -> ironrdp_server::ErrorInfoDisconnectHandle {
        self.primary.error_info_disconnect_handle()
    }
}

/// Pure routing decision: vsock routes to the plain server iff one exists;
/// every other transport (and every vsock connection on a single-server
/// setup) routes to the primary. The `AcceptorMode` is accepted for future
/// per-mode routing but currently does not influence the decision — WSS
/// PreAuthenticated connections still belong on the primary (their TLS was
/// terminated by the WSS layer, but the RDP security negotiation is the
/// primary's).
fn route_connection(transport: &str, _mode: AcceptorMode, has_plain: bool) -> ServerRoute {
    match transport {
        "vsock" if has_plain => ServerRoute::Plain,
        _ => ServerRoute::Primary,
    }
}

impl AcceptDispatcher {
    /// Build everything from the deployment and run the accept loop.
    ///
    /// The deployment's `build_listeners`, `build_handler`, and
    /// `shutdown_signal` methods are each called exactly once before the loop
    /// starts.
    pub async fn run(
        mut deployment: impl AcceptDeployment,
        servers: &mut ServerPair,
    ) -> Result<()> {
        let dep_name = deployment.name();
        let mut listeners = deployment.build_listeners().await?;
        let mut handler = deployment.build_handler();
        let mut shutdown = deployment.shutdown_signal();

        if listeners.is_empty() {
            anyhow::bail!("AcceptDispatcher ({dep_name}): no listeners configured");
        }

        let transport_summary: Vec<&'static str> =
            listeners.iter().map(|l| l.transport_name()).collect();
        info!(
            deployment = dep_name,
            transports = ?transport_summary,
            count = listeners.len(),
            "Accept dispatcher started"
        );

        loop {
            // Build accept futures across all listeners; race them against shutdown.
            let mut accept_futures: FuturesUnordered<_> = listeners
                .iter_mut()
                .enumerate()
                .map(|(idx, l)| {
                    let name = l.transport_name();
                    async move { (idx, name, l.accept().await) }
                })
                .collect();

            let (transport_name, accept_result) = tokio::select! {
                () = &mut shutdown => {
                    info!(deployment = dep_name, "Shutdown signal received: stopping accept dispatcher");
                    return Ok(());
                }
                Some((_idx, name, result)) = accept_futures.next() => (name, result),
            };
            // Explicit drop (not just end-of-scope) to release the &mut borrow
            // on listeners before the drain step below needs its own &mut.
            drop(accept_futures);

            match accept_result {
                Ok(Some(accepted)) => {
                    let AcceptedConnection { peer, stream, mode } = accepted;
                    debug!(
                        transport = transport_name,
                        peer = %peer.to_display(),
                        ?mode,
                        "Connection accepted"
                    );

                    // Dead-client wedge mitigation: a
                    // silent peer parks the acceptor's first read inside this
                    // serial loop and blacks out every listener until it goes
                    // away. Wrap the stream so the FIRST client byte must
                    // arrive within the deadline; once the exchange starts the
                    // deadline is disarmed and idle sessions are unaffected.
                    let peer_display = peer.to_display();
                    let stream: Box<dyn AsyncRdpStream> =
                        Box::new(handshake_deadline::HandshakeDeadlineStream::new(
                            stream,
                            handshake_deadline::DEFAULT_HANDSHAKE_DEADLINE,
                        ));

                    // on_accept_async: PAM peer-IP setup, ClientConnected event,
                    // cache client state for matching on_disconnected.
                    let accept_ok = handler.on_accept_async(&peer).await;
                    if !accept_ok {
                        debug!(
                            transport = transport_name,
                            peer = %peer.to_display(),
                            "Connection rejected by handler"
                        );
                        continue;
                    }

                    let start = Instant::now();
                    // Per-transport security routing: pick the serving server
                    // BEFORE the connection runs, and let the deployment
                    // retarget per-server wiring (event sender) at it.
                    let route = servers.route(transport_name, mode);
                    deployment.on_server_routed(route);
                    let rdp_server = servers.server_for(route);
                    let conn_result = match mode {
                        AcceptorMode::Standard => {
                            rdp_server.run_connection(stream).await
                        }
                        AcceptorMode::PreAuthenticated => {
                            // Stream is already TLS-terminated (typically WSS); skip the
                            // IronRDP-managed TLS upgrade. Upstream PR #1281 reshaped this
                            // from a dedicated run_connection_pre_authenticated method into
                            // run_connection_with + TransportTls::AlreadyDone.
                            rdp_server
                                .run_connection_with(
                                    stream,
                                    ironrdp_server::TransportTls::AlreadyDone,
                                )
                                .await
                        }
                    };
                    let duration = start.elapsed();

                    // Surface deadline-based aborts distinctly from ordinary
                    // handshake failures — the wedge mitigation working as
                    // designed is worth seeing in logs at a glance.
                    if let Err(ref e) = conn_result {
                        if e.to_string().contains("handshake deadline elapsed") {
                            handshake_deadline::log_deadline_rejection(&peer_display, duration);
                        }
                    }

                    // on_disconnected_async: classify error, emit ClientDisconnected,
                    // prune PAM rate limits, check Portal validity.
                    let action = handler
                        .on_disconnected_async(&peer, duration, conn_result.as_ref().err())
                        .await;

                    if matches!(action, PostConnectionAction::Stop) {
                        info!("Handler requested stop: terminating accept loop");
                        return Ok(());
                    }

                    // #57-adjacent: run_connection() above occupies this loop for
                    // the entire session, so the OS backlog is the only thing
                    // absorbing connection attempts that arrive while we're busy.
                    // A client that gives up before we ever get back to accept()
                    // leaves an unaccepted, already-CLOSE-WAIT socket sitting
                    // there — under sustained rapid reconnects that fills the
                    // backlog and starves every later attempt. Drain and
                    // immediately drop anything that queued up during the
                    // session we just finished, rather than serving each one a
                    // full (likely-abandoned) connection attempt in turn.
                    // Bounded so a genuine connection flood can't stall the
                    // loop indefinitely; each attempt costs at most 1ms.
                    let mut drained = 0u32;
                    'drain: for l in listeners.iter_mut() {
                        for _ in 0..32 {
                            match tokio::time::timeout(Duration::from_millis(1), l.accept()).await {
                                Ok(Ok(Some(_))) => drained += 1,
                                _ => continue 'drain,
                            }
                        }
                    }
                    if drained > 0 {
                        warn!(
                            deployment = dep_name,
                            drained,
                            "Dropped stale connection attempts queued while busy with the prior session"
                        );
                    }
                }
                Ok(None) => {
                    warn!(
                        transport = transport_name,
                        "Listener gracefully closed; continuing with remaining listeners"
                    );
                    // TODO Phase 4+: remove this listener from the Vec rather than re-poll it.
                    // For Phase 1 (single TCP listener), graceful close is server shutdown.
                    return Ok(());
                }
                Err(e) => {
                    error!(transport = transport_name, error = %e, "Accept failed");
                    // Transient errors are expected (e.g. EMFILE). Continue.
                }
            }
        }
    }
}

/// Re-export of upstream lifecycle types for convenience.
pub use ironrdp_server::ConnectionHandler as UpstreamConnectionHandler;
pub use ironrdp_server::PostConnectionAction as UpstreamPostConnectionAction;

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::sync::{Mutex, oneshot};

    use super::*;
    use crate::transport::{
        handler::OnDisconnectFn,
        listener::{AcceptedConnection, AsyncRdpStream, PeerAddr, TransportError},
    };

    /// Mock listener that yields one prepared connection then closes. Useful
    /// for driving the dispatcher in tests without sockets.
    struct MockListener {
        stream: Option<Box<dyn AsyncRdpStream>>,
        peer: PeerAddr,
    }

    #[test]
    fn routes_vsock_to_plain_only_when_plain_exists() {
        use crate::transport::listener::AcceptorMode;
        // Dual-server (vsock active, security_mode != rdp): vsock → Plain.
        assert!(matches!(
            route_connection("vsock", AcceptorMode::Standard, true),
            ServerRoute::Plain
        ));
        // Single-server (security_mode = rdp): vsock → Primary (which IS
        // plain — identical wire behavior).
        assert!(matches!(
            route_connection("vsock", AcceptorMode::Standard, false),
            ServerRoute::Primary
        ));
        // All other transports stay on the primary regardless.
        for transport in ["tcp", "unix", "websocket"] {
            assert!(matches!(
                route_connection(transport, AcceptorMode::Standard, true),
                ServerRoute::Primary
            ));
            assert!(matches!(
                route_connection(transport, AcceptorMode::PreAuthenticated, true),
                ServerRoute::Primary
            ));
        }
        // WSS (PreAuthenticated) stays primary even on the dual setup: its
        // TLS was terminated by the WSS listener, and the RDP security
        // negotiation belongs to the primary.
        assert!(matches!(
            route_connection("websocket", AcceptorMode::PreAuthenticated, true),
            ServerRoute::Primary
        ));
    }

    #[async_trait::async_trait]
    impl Listener for MockListener {
        fn transport_name(&self) -> &'static str {
            "mock"
        }

        async fn accept(&mut self) -> Result<Option<AcceptedConnection>, TransportError> {
            match self.stream.take() {
                Some(stream) => Ok(Some(AcceptedConnection::standard(
                    self.peer.clone(),
                    stream,
                ))),
                None => {
                    // After our single connection, block forever (real listener
                    // would block on `accept()`; we simulate by never resolving).
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
    }

    /// Minimal deployment for build-sequence verification. Does NOT exercise the
    /// dispatcher loop (that requires a real RdpServer); only checks that
    /// `build_listeners` and `build_handler` and `shutdown_signal` are wired up.
    struct MockDeployment {
        listener: Option<Box<dyn Listener>>,
        event_tx: tokio::sync::mpsc::UnboundedSender<crate::dbus::ServerEvent>,
        shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        shutdown_rx: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    }

    impl MockDeployment {
        fn new(
            listener: Box<dyn Listener>,
            event_tx: tokio::sync::mpsc::UnboundedSender<crate::dbus::ServerEvent>,
        ) -> Self {
            let (tx, rx) = oneshot::channel();
            Self {
                listener: Some(listener),
                event_tx,
                shutdown_tx: Arc::new(Mutex::new(Some(tx))),
                shutdown_rx: Arc::new(Mutex::new(Some(rx))),
            }
        }

        async fn fire_shutdown(&self) {
            if let Some(tx) = self.shutdown_tx.lock().await.take() {
                let _ = tx.send(());
            }
        }
    }

    #[async_trait::async_trait]
    impl AcceptDeployment for MockDeployment {
        fn name(&self) -> &'static str {
            "mock"
        }

        async fn build_listeners(&mut self) -> Result<Vec<Box<dyn Listener>>> {
            Ok(self.listener.take().into_iter().collect())
        }

        fn build_handler(&mut self) -> LamcoConnectionHandler {
            let cb: OnDisconnectFn = Arc::new(|_served| Box::pin(async { true }));
            LamcoConnectionHandler::new(
                self.event_tx.clone(),
                None,
                cb,
                Arc::new(tokio::sync::broadcast::channel::<()>(1).0),
            )
        }

        fn shutdown_signal(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
            let rx_slot = self.shutdown_rx.clone();
            Box::pin(async move {
                let rx = rx_slot.lock().await.take();
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
            })
        }
    }

    #[tokio::test]
    async fn deployment_build_sequence() {
        // Construct a mock listener with one prepared duplex stream pair.
        let (a, _b) = tokio::io::duplex(64);
        let peer = PeerAddr::Tcp("127.0.0.1:0".parse().unwrap());
        let mock_listener = MockListener {
            stream: Some(Box::new(a)),
            peer,
        };
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();

        let mut deployment = MockDeployment::new(Box::new(mock_listener), event_tx);

        // Verify each build_* method works as expected.
        assert_eq!(deployment.name(), "mock");

        let listeners = deployment.build_listeners().await.unwrap();
        assert_eq!(listeners.len(), 1);
        assert_eq!(listeners[0].transport_name(), "mock");

        let _handler = deployment.build_handler();

        let shutdown = deployment.shutdown_signal();
        // Firing the oneshot resolves the future.
        deployment.fire_shutdown().await;
        tokio::time::timeout(Duration::from_millis(100), shutdown)
            .await
            .expect("shutdown_signal should resolve after fire_shutdown");
    }
}
