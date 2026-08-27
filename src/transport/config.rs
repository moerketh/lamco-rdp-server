//! Transport configuration materialization.
//!
//! Phase 1 introduces the `[server.transports]` table. Only `[server.transports.tcp]`
//! is recognized in Phase 1; back-compat is preserved by synthesizing a default-enabled
//! TCP transport from the existing `server.listen_addr` field when the new table is
//! absent. All existing configs continue to work unchanged.
//!
//! Phases 2/3a/3b/4 add `unix`, `vsock`, and `websocket` subtables behind Cargo
//! features.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::listener::{Listener, TcpListenerImpl, detect_hyperv};

/// Root TOML section: `[server.transports.*]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportsConfig {
    #[serde(default)]
    pub tcp: Option<TcpTransportConfig>,
    /// AF_VSOCK transport (Hyper-V Enhanced Session Mode + future QEMU guest).
    /// Auto-enabled on Hyper-V detection if absent. See `VsockTransportConfig`.
    #[serde(default)]
    pub vsock: Option<VsockTransportConfig>,
    /// WebSocket+RDCleanPath transport for browser/WASM clients
    /// (lamco-rdp-wasm and other RDCleanPath-aware clients). Opt-in via
    /// `enabled = true`. Requires the `websocket` Cargo feature.
    #[serde(default)]
    pub websocket: Option<crate::transport::config::WebSocketTransportConfigRef>,
}

/// Public re-export shim so the TOML schema field can live here while the
/// concrete `WebSocketTransportConfig` type lives in
/// `crate::transport::websocket` (only present when the `websocket` feature
/// is enabled). The shim has identical serde shape; the deployment layer
/// translates one into the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketTransportConfigRef {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ws_listen_addr")]
    pub listen_addr: String,
    #[serde(default)]
    pub cert_path: Option<std::path::PathBuf>,
    #[serde(default)]
    pub key_path: Option<std::path::PathBuf>,
    /// Path to a file holding the RDCleanPath `proxy_auth` shared secret for the
    /// direct WebSocket path (a trailing newline is trimmed). When unset, the
    /// listener fails closed and rejects all direct RDCleanPath connections.
    #[serde(default)]
    pub proxy_auth_secret_path: Option<std::path::PathBuf>,
    /// Development-only: accept any `proxy_auth` token on the WebSocket path.
    /// Never enable on a network-reachable listener.
    #[serde(default)]
    pub proxy_auth_allow_insecure: bool,
}

impl Default for WebSocketTransportConfigRef {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_ws_listen_addr(),
            cert_path: None,
            key_path: None,
            proxy_auth_secret_path: None,
            proxy_auth_allow_insecure: false,
        }
    }
}

fn default_ws_listen_addr() -> String {
    "[::]:3390".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpTransportConfig {
    /// Default true. Set to false to disable TCP without removing the table.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// If set, overrides `server.listen_addr`. If absent, uses `server.listen_addr`.
    #[serde(default)]
    pub listen_addr: Option<String>,
}

impl Default for TcpTransportConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen_addr: None,
        }
    }
}

/// AF_VSOCK transport configuration.
///
/// `enabled` is tri-state:
/// - `Some(true)`  — always enable, regardless of host
/// - `Some(false)` — always disable
/// - `None` (default) — auto-enable when Hyper-V is detected via DMI, otherwise off
///
/// If the entire `[server.transports.vsock]` subtable is absent, the
/// auto-detect behavior is also used with default port 3389.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VsockTransportConfig {
    /// `None` = auto-detect Hyper-V. See struct-level docs.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// AF_VSOCK port to listen on. Defaults to 3389 (parallel to TCP).
    #[serde(default = "default_vsock_port")]
    pub port: u32,
    /// Peer context IDs allowed to open an Enhanced Session. Absent means
    /// `[2]` (`VMADDR_CID_HOST`), which is correct for Hyper-V.
    ///
    /// Only widen this for a hypervisor that presents a different CID. The
    /// Enhanced Session path performs no authentication at all, so this list
    /// *is* the access control for the transport; the in-guest loopback CID
    /// (1) is refused regardless of what is configured here.
    #[serde(default)]
    pub allowed_cids: Option<Vec<u32>>,
}

impl Default for VsockTransportConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            port: default_vsock_port(),
            allowed_cids: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_vsock_port() -> u32 {
    3389
}

/// Resolve the configured peer allowlist, falling back to the hypervisor-only
/// default. An explicitly empty list is treated as unset rather than as "refuse
/// everything", which would silently disable the transport.
#[cfg(feature = "vsock")]
fn resolve_allowed_cids(configured: Option<&[u32]>) -> Vec<u32> {
    match configured {
        Some(cids) if !cids.is_empty() => cids.to_vec(),
        _ => super::listener::default_allowed_cids(),
    }
}

impl TransportsConfig {
    /// Resolve the effective set of configured transports.
    ///
    /// Back-compat for TCP: if `[server.transports.tcp]` is absent, synthesize
    /// a default-enabled TCP entry pointing at `server.listen_addr`. Configs
    /// that never mention `[server.transports]` continue to work.
    ///
    /// vsock (Phase 3a): tri-state per `VsockTransportConfig`. `None` =
    /// auto-detect Hyper-V; explicit `enabled` overrides detection.
    pub fn resolve(&self, server_listen_addr: &str) -> Result<ResolvedTransports> {
        // -- TCP --
        let tcp = match &self.tcp {
            Some(cfg) if cfg.enabled => {
                let addr_str = cfg.listen_addr.as_deref().unwrap_or(server_listen_addr);
                let addr: SocketAddr = addr_str.parse().with_context(|| {
                    format!("invalid server.transports.tcp.listen_addr: {addr_str}")
                })?;
                Some(ResolvedTcpTransport { listen_addr: addr })
            }
            Some(_) => None, // explicitly disabled
            None => {
                let addr: SocketAddr = server_listen_addr
                    .parse()
                    .with_context(|| format!("invalid server.listen_addr: {server_listen_addr}"))?;
                Some(ResolvedTcpTransport { listen_addr: addr })
            }
        };

        // -- vsock (tri-state with Hyper-V auto-detection) --
        let vsock = match &self.vsock {
            Some(cfg) => match cfg.enabled {
                Some(true) => {
                    info!(port = cfg.port, "vsock listener: explicitly enabled");
                    Some(ResolvedVsockTransport {
                        port: cfg.port,
                        allowed_cids: resolve_allowed_cids(cfg.allowed_cids.as_deref()),
                    })
                }
                Some(false) => {
                    info!("vsock listener: explicitly disabled");
                    None
                }
                None => {
                    if detect_hyperv() {
                        info!(
                            port = cfg.port,
                            "vsock listener: Hyper-V detected, auto-enabling"
                        );
                        Some(ResolvedVsockTransport {
                            port: cfg.port,
                            allowed_cids: resolve_allowed_cids(cfg.allowed_cids.as_deref()),
                        })
                    } else {
                        info!("vsock listener: not Hyper-V, leaving disabled");
                        None
                    }
                }
            },
            None => {
                if detect_hyperv() {
                    let port = default_vsock_port();
                    info!(
                        port,
                        "vsock listener: Hyper-V detected, auto-enabling with defaults"
                    );
                    Some(ResolvedVsockTransport {
                        port,
                        allowed_cids: super::listener::default_allowed_cids(),
                    })
                } else {
                    None
                }
            }
        };

        // -- websocket (opt-in via enabled=true; no auto-detection) --
        let websocket = match &self.websocket {
            Some(cfg) if cfg.enabled => {
                let addr: SocketAddr = cfg.listen_addr.parse().with_context(|| {
                    format!(
                        "invalid server.transports.websocket.listen_addr: {}",
                        cfg.listen_addr
                    )
                })?;
                info!(%addr, "WebSocket listener: configured");
                Some(ResolvedWebSocketTransport {
                    listen_addr: addr,
                    cert_path: cfg.cert_path.clone(),
                    key_path: cfg.key_path.clone(),
                    proxy_auth_secret_path: cfg.proxy_auth_secret_path.clone(),
                    proxy_auth_allow_insecure: cfg.proxy_auth_allow_insecure,
                })
            }
            Some(_) => {
                info!("WebSocket listener: explicitly disabled");
                None
            }
            None => None,
        };

        if tcp.is_none() && vsock.is_none() && websocket.is_none() {
            anyhow::bail!(
                "no transports enabled; at least one [server.transports.*] section must have enabled=true"
            );
        }

        Ok(ResolvedTransports {
            tcp,
            vsock,
            websocket,
        })
    }

    /// Phase 1 compatibility shim. Phase 3a renamed this to `resolve`; the
    /// old name is preserved as a thin wrapper to keep WlrDirectDeployment
    /// callers stable. Prefer `resolve`.
    pub fn resolve_phase1(&self, server_listen_addr: &str) -> Result<ResolvedTransports> {
        self.resolve(server_listen_addr)
    }
}

/// Validated transport configuration, ready to construct listeners from.
#[derive(Debug)]
pub struct ResolvedTransports {
    pub tcp: Option<ResolvedTcpTransport>,
    pub vsock: Option<ResolvedVsockTransport>,
    pub websocket: Option<ResolvedWebSocketTransport>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTcpTransport {
    pub listen_addr: SocketAddr,
}

#[derive(Debug, Clone)]
pub struct ResolvedVsockTransport {
    pub port: u32,
    /// Resolved peer allowlist; never empty. See [`VsockTransportConfig::allowed_cids`].
    pub allowed_cids: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct ResolvedWebSocketTransport {
    pub listen_addr: SocketAddr,
    /// Optional per-transport TLS cert override (if absent, deployment uses
    /// the server-default cert from `security.cert_path`).
    pub cert_path: Option<std::path::PathBuf>,
    pub key_path: Option<std::path::PathBuf>,
    /// Path to the `proxy_auth` shared-secret file for the direct WebSocket
    /// path. `None` means fail closed (reject all direct connections).
    pub proxy_auth_secret_path: Option<std::path::PathBuf>,
    /// Development-only: disable `proxy_auth` validation on the WebSocket path.
    pub proxy_auth_allow_insecure: bool,
}

impl ResolvedTransports {
    /// Bind all configured listeners and return them as a homogeneous Vec for
    /// the `AcceptDispatcher`.
    ///
    /// WebSocket listeners are built by the caller (deployment layer) because
    /// they need a TLS identity (`TlsAcceptor` + cert chain) which is the
    /// deployment's concern. This method binds only the listeners that need
    /// no extra context.
    pub async fn build_listeners(&self) -> Result<Vec<Box<dyn Listener>>> {
        let mut out: Vec<Box<dyn Listener>> = Vec::new();
        if let Some(tcp) = &self.tcp {
            let l = TcpListenerImpl::bind(tcp.listen_addr)
                .await
                .with_context(|| format!("failed to bind TCP listener on {}", tcp.listen_addr))?;
            out.push(Box::new(l));
        }
        #[cfg(feature = "vsock")]
        if let Some(vsock) = &self.vsock {
            let l =
                super::listener::VsockListenerImpl::bind(vsock.port, vsock.allowed_cids.clone())
                    .with_context(|| {
                        format!("failed to bind vsock listener on port {}", vsock.port)
                    })?;
            out.push(Box::new(l));
        }
        #[cfg(not(feature = "vsock"))]
        if self.vsock.is_some() {
            warn!(
                "vsock transport configured/auto-detected but `vsock` Cargo feature is disabled — ignoring"
            );
        }
        // WebSocket listener is constructed in the deployment layer because it
        // needs a TLS identity. The `websocket` field here just carries the
        // resolved config; the deployment reads it and calls
        // WebSocketListenerImpl::bind itself.
        #[cfg(not(feature = "websocket"))]
        if self.websocket.is_some() {
            warn!(
                "WebSocket transport configured but `websocket` Cargo feature is disabled — ignoring"
            );
        }
        Ok(out)
    }
}
