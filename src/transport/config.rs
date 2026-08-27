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
/// `enabled` must be set explicitly; absent means off.
/// - `Some(true)`  — enable
/// - `Some(false)` / `None` / subtable absent — disabled
///
/// This transport was previously auto-enabled whenever `/sys/class/dmi` looked
/// like Hyper-V. It no longer is. The Enhanced Session path it serves performs
/// no TLS, no CredSSP and no credential check, and cannot authenticate at all —
/// vmms relays empty Client Info credentials because the user was already
/// authenticated against the host. Switching on an unauthenticated listener is
/// an operator decision, not something to infer from two DMI strings that a
/// hypervisor or container runtime can set. Hyper-V detection now only logs a
/// suggestion at startup.
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
/// Point the operator at the setting when this looks like a Hyper-V guest.
///
/// Deliberately does not enable anything. `detect_hyperv` reads two
/// operator-controllable DMI strings, and the transport it would switch on is
/// unauthenticated by construction — that is not a decision two `/sys` reads
/// should make. See [`VsockTransportConfig`].
fn suggest_vsock_if_hyperv() {
    if detect_hyperv() {
        info!(
            "Hyper-V detected. The vsock listener (Enhanced Session) is NOT enabled by              default because that transport performs no authentication. To enable it, set              `[server.transports.vsock] enabled = true`."
        );
    } else {
        info!("vsock listener: not enabled");
    }
}

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
    /// vsock: opt-in only. Absent, `enabled = false`, or an unset `enabled` all
    /// leave it disabled; Hyper-V detection merely logs a suggestion. See
    /// [`VsockTransportConfig`] for why this is not auto-enabled.
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
                    suggest_vsock_if_hyperv();
                    None
                }
            },
            None => {
                suggest_vsock_if_hyperv();
                None
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
            // The startup exposure guard in server::mod only inspects the TCP bind,
            // and `has_routable_inet_listener` classifies AF_VSOCK as non-routable by
            // construction — so this listener would otherwise come up silently. It
            // needs its own warning, and a different one: auth_method is irrelevant
            // here. The Enhanced Session path performs no TLS, no CredSSP and no
            // credential check, and *cannot* — vmms relays a Client Info PDU with
            // empty username and password because the user was already authenticated
            // against the host. The peer allowlist is the whole of the access control.
            warn!(
                port = vsock.port,
                allowed_cids = ?vsock.allowed_cids,
                "Hyper-V Enhanced Session listener active: connections on this transport are                  NOT authenticated and NOT encrypted by this server, and no setting can change                  that. Access is controlled solely by the peer CID allowlist."
            );
            let l =
                super::listener::VsockListenerImpl::bind(vsock.port, vsock.allowed_cids.clone())
                    .with_context(|| {
                        format!("failed to bind vsock listener on port {}", vsock.port)
                    })?;
            out.push(Box::new(l));
        }
        #[cfg(not(feature = "vsock"))]
        if self.vsock.is_some() {
            warn!("vsock transport configured but `vsock` Cargo feature is disabled — ignoring");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(vsock: Option<VsockTransportConfig>) -> TransportsConfig {
        TransportsConfig {
            vsock,
            ..Default::default()
        }
    }

    fn resolved(vsock: Option<VsockTransportConfig>) -> ResolvedTransports {
        cfg(vsock).resolve("127.0.0.1:3389").expect("resolve")
    }

    /// The transport is unauthenticated by construction, so it must never come up
    /// unasked. Previously two `/sys/class/dmi` reads were enough to enable it —
    /// strings a hypervisor or container runtime controls, and which a container
    /// inherits from its host VM.
    #[test]
    fn vsock_is_off_unless_explicitly_enabled() {
        assert!(
            resolved(None).vsock.is_none(),
            "absent subtable must stay off"
        );

        assert!(
            resolved(Some(VsockTransportConfig::default()))
                .vsock
                .is_none(),
            "unset `enabled` must stay off, regardless of the host"
        );

        assert!(
            resolved(Some(VsockTransportConfig {
                enabled: Some(false),
                ..Default::default()
            }))
            .vsock
            .is_none(),
            "explicit false must stay off"
        );
    }

    #[test]
    fn vsock_enabled_uses_the_host_only_allowlist_by_default() {
        let got = resolved(Some(VsockTransportConfig {
            enabled: Some(true),
            ..Default::default()
        }))
        .vsock
        .expect("explicitly enabled");

        assert_eq!(got.port, default_vsock_port());
        assert_eq!(
            got.allowed_cids,
            super::super::listener::default_allowed_cids()
        );
    }

    #[test]
    fn vsock_honours_a_configured_allowlist_but_not_an_empty_one() {
        let widened = resolved(Some(VsockTransportConfig {
            enabled: Some(true),
            allowed_cids: Some(vec![2, 9]),
            ..Default::default()
        }))
        .vsock
        .expect("enabled");
        assert_eq!(widened.allowed_cids, vec![2, 9]);

        // An empty list would otherwise refuse every peer, silently disabling the
        // transport rather than doing what the operator meant.
        let empty = resolved(Some(VsockTransportConfig {
            enabled: Some(true),
            allowed_cids: Some(vec![]),
            ..Default::default()
        }))
        .vsock
        .expect("enabled");
        assert_eq!(
            empty.allowed_cids,
            super::super::listener::default_allowed_cids()
        );
    }
}
