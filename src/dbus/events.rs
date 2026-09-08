//! Server Event Relay
//!
//! Decouples the server from D-Bus awareness. The server emits `ServerEvent`
//! on an mpsc channel. When the D-Bus service is active, a relay task
//! converts events to D-Bus signals. When D-Bus is not active, events
//! are simply dropped (sender has no receiver).
//!
//! This pattern also supports future consumers: metrics, logging, CLI tool.

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::object_server::InterfaceRef;

use super::{OBJECT_PATH, SharedServerState, manager::RdpServerManager};

/// Events emitted by the server runtime for external consumption.
///
/// These are fire-and-forget — the server sends them without knowing
/// whether anyone is listening.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// Server status transition
    StatusChanged {
        old: String,
        new: String,
        message: String,
    },

    /// New RDP client connected
    ClientConnected {
        client_id: String,
        peer_address: String,
        timestamp: u64,
    },

    /// RDP client disconnected
    ClientDisconnected {
        client_id: String,
        reason: String,
        duration_seconds: u64,
    },

    /// Configuration reloaded
    ConfigReloaded { config_path: String },

    /// Session health changed (from health monitor)
    SessionHealthChanged {
        old_health: String,
        new_health: String,
        detail: String,
    },

    /// Session type determined (emitted once during initialization)
    SessionTypeChanged { session_type: String },

    /// Periodic performance metrics update (from SnapshotCollector)
    PerformanceUpdated {
        fps: u32,
        latency_ms: f32,
        queue_depth: u32,
        encoder_backend: String,
        activity_level: String,
        /// Encoding adaptation: current QP (0 = adaptation not active)
        current_qp: u32,
        /// Encoding adaptation: whether adaptive QP is enabled
        adaptation_enabled: bool,
        /// Damage source: "compositor", "pixel-diff", or "full-frame"
        damage_source: String,
        /// Number of registered health sensors
        sensor_count: u32,
        /// Current bitrate in kbps (from encoder)
        bitrate_kbps: u32,
        /// Per-subsystem health: video
        health_video: String,
        /// Per-subsystem health: input
        health_input: String,
        /// Per-subsystem health: clipboard
        health_clipboard: String,
        /// Per-subsystem health: session
        health_session: String,
    },
}

/// Create an event channel for server events.
///
/// Returns (sender, receiver). The sender is cloned and distributed to
/// subsystems that need to emit events. The receiver is consumed by the
/// relay task.
pub fn event_channel() -> (
    mpsc::UnboundedSender<ServerEvent>,
    mpsc::UnboundedReceiver<ServerEvent>,
) {
    mpsc::unbounded_channel()
}

/// Start the D-Bus signal relay task.
///
/// Consumes events from the channel and emits D-Bus signals via the
/// registered `RdpServerManager` interface. Also updates the shared
/// server state for property changes.
///
/// Returns the task handle.
pub fn start_signal_relay(
    connection: zbus::Connection,
    mut event_rx: mpsc::UnboundedReceiver<ServerEvent>,
    state: SharedServerState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("D-Bus signal relay started");

        while let Some(event) = event_rx.recv().await {
            debug!("Relaying server event: {event:?}");

            match &event {
                ServerEvent::StatusChanged { old, new, message } => {
                    // Update shared state
                    {
                        let mut s = state.write().await;
                        s.status = super::ServerStatus::from(new.as_str());
                    }

                    // Emit D-Bus signal
                    if let Err(e) = emit_state_changed(&connection, old, new, message).await {
                        warn!("Failed to emit server_state_changed signal: {e}");
                    }
                }

                ServerEvent::ClientConnected {
                    client_id,
                    peer_address,
                    timestamp,
                } => {
                    // Update shared state
                    {
                        let mut s = state.write().await;
                        s.active_connections += 1;
                    }

                    // Track the client so GetConnections / disconnect_client
                    // operate on real data. Nothing populated this list before:
                    // add_client was only ever called from tests, so the GUI's
                    // connection view was permanently empty in production.
                    let iface_ref: Result<
                        zbus::object_server::InterfaceRef<RdpServerManager>,
                        zbus::Error,
                    > = connection.object_server().interface(OBJECT_PATH).await;
                    if let Ok(iface_ref) = iface_ref {
                        let manager = iface_ref.get().await;
                        manager
                            .add_client(super::ClientInfo {
                                client_id: client_id.clone(),
                                peer_address: peer_address.clone(),
                                username: String::new(),
                                connected_at: *timestamp,
                            })
                            .await;
                    }

                    if let Err(e) =
                        emit_client_connected(&connection, client_id, peer_address, *timestamp)
                            .await
                    {
                        warn!("Failed to emit client_connected signal: {e}");
                    }
                }

                ServerEvent::ClientDisconnected {
                    client_id,
                    reason,
                    duration_seconds,
                } => {
                    // Update shared state
                    {
                        let mut s = state.write().await;
                        s.active_connections = s.active_connections.saturating_sub(1);
                    }

                    // Keep the tracked client list in sync (see ClientConnected).
                    let iface_ref: Result<
                        zbus::object_server::InterfaceRef<RdpServerManager>,
                        zbus::Error,
                    > = connection.object_server().interface(OBJECT_PATH).await;
                    if let Ok(iface_ref) = iface_ref {
                        let manager = iface_ref.get().await;
                        manager.remove_client(client_id).await;
                    }

                    if let Err(e) =
                        emit_client_disconnected(&connection, client_id, reason, *duration_seconds)
                            .await
                    {
                        warn!("Failed to emit client_disconnected signal: {e}");
                    }
                }

                ServerEvent::ConfigReloaded { config_path } => {
                    if let Err(e) = emit_config_reloaded(&connection, config_path).await {
                        warn!("Failed to emit config_reloaded signal: {e}");
                    }
                }

                ServerEvent::SessionTypeChanged { session_type } => {
                    // Update shared state
                    {
                        let mut s = state.write().await;
                        s.session_type.clone_from(session_type);
                    }
                    debug!("Session type set: {session_type}");
                }

                ServerEvent::SessionHealthChanged {
                    old_health,
                    new_health,
                    detail,
                } => {
                    // Health changes map to status_changed with health-prefixed values
                    let message = format!("Session health: {detail}");
                    let old = format!("running-{old_health}");
                    let new = format!("running-{new_health}");

                    if let Err(e) = emit_state_changed(&connection, &old, &new, &message).await {
                        warn!("Failed to emit health state_changed signal: {e}");
                    }
                }

                ServerEvent::PerformanceUpdated {
                    fps,
                    latency_ms,
                    queue_depth,
                    encoder_backend,
                    activity_level,
                    current_qp,
                    adaptation_enabled,
                    damage_source,
                    sensor_count,
                    bitrate_kbps,
                    health_video,
                    health_input,
                    health_clipboard,
                    health_session,
                } => {
                    if let Err(e) = emit_performance_updated(
                        &connection,
                        *fps,
                        *latency_ms,
                        *queue_depth,
                        encoder_backend,
                        activity_level,
                        *current_qp,
                        *adaptation_enabled,
                        damage_source,
                        *sensor_count,
                        *bitrate_kbps,
                        health_video,
                        health_input,
                        health_clipboard,
                        health_session,
                    )
                    .await
                    {
                        debug!("Failed to emit performance_updated signal: {e}");
                    }
                }
            }
        }

        info!("D-Bus signal relay stopped (event channel closed)");
    })
}

async fn emit_state_changed(
    connection: &zbus::Connection,
    old: &str,
    new: &str,
    message: &str,
) -> Result<(), zbus::Error> {
    let iface_ref: InterfaceRef<RdpServerManager> =
        connection.object_server().interface(OBJECT_PATH).await?;

    RdpServerManager::server_state_changed(iface_ref.signal_emitter(), old, new, message).await
}

async fn emit_client_connected(
    connection: &zbus::Connection,
    client_id: &str,
    peer_address: &str,
    timestamp: u64,
) -> Result<(), zbus::Error> {
    let iface_ref: InterfaceRef<RdpServerManager> =
        connection.object_server().interface(OBJECT_PATH).await?;

    RdpServerManager::client_connected(
        iface_ref.signal_emitter(),
        client_id,
        peer_address,
        timestamp,
    )
    .await
}

async fn emit_client_disconnected(
    connection: &zbus::Connection,
    client_id: &str,
    reason: &str,
    duration_seconds: u64,
) -> Result<(), zbus::Error> {
    let iface_ref: InterfaceRef<RdpServerManager> =
        connection.object_server().interface(OBJECT_PATH).await?;

    RdpServerManager::client_disconnected(
        iface_ref.signal_emitter(),
        client_id,
        reason,
        duration_seconds,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors D-Bus signal parameter list"
)]
async fn emit_performance_updated(
    connection: &zbus::Connection,
    fps: u32,
    latency_ms: f32,
    queue_depth: u32,
    encoder_backend: &str,
    activity_level: &str,
    current_qp: u32,
    adaptation_enabled: bool,
    damage_source: &str,
    sensor_count: u32,
    bitrate_kbps: u32,
    health_video: &str,
    health_input: &str,
    health_clipboard: &str,
    health_session: &str,
) -> Result<(), zbus::Error> {
    let iface_ref: InterfaceRef<RdpServerManager> =
        connection.object_server().interface(OBJECT_PATH).await?;

    RdpServerManager::performance_updated(
        iface_ref.signal_emitter(),
        fps,
        latency_ms,
        queue_depth,
        encoder_backend,
        activity_level,
        current_qp,
        adaptation_enabled,
        damage_source,
        sensor_count,
        bitrate_kbps,
        health_video,
        health_input,
        health_clipboard,
        health_session,
    )
    .await
}

async fn emit_config_reloaded(
    connection: &zbus::Connection,
    config_path: &str,
) -> Result<(), zbus::Error> {
    let iface_ref: InterfaceRef<RdpServerManager> =
        connection.object_server().interface(OBJECT_PATH).await?;

    RdpServerManager::config_reloaded(iface_ref.signal_emitter(), config_path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_channel_creation() {
        let (tx, _rx) = event_channel();
        // Sending without receiver should not panic
        let _ = tx.send(ServerEvent::StatusChanged {
            old: "stopped".into(),
            new: "running".into(),
            message: "test".into(),
        });
    }

    #[test]
    fn test_server_event_debug() {
        let event = ServerEvent::ClientConnected {
            client_id: "test-1".into(),
            peer_address: "192.168.1.1:3389".into(),
            timestamp: 123456,
        };
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("test-1"));
    }
}
