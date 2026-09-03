//! Compositor Health Sources — active push-based health monitoring
//!
//! Each compositor family has different D-Bus signals and health indicators.
//! `CompositorHealthSource` abstracts this: implementations subscribe to
//! compositor-specific signals and push `HealthEvent` values to the health monitor.
//!
//! This is the ACTIVE (push) complement to `HealthSensor`'s PASSIVE (pull) metrics.
//!
//! # Implementations
//!
//! | Source | Compositor | Signals |
//! |--------|-----------|---------|
//! | `DbusNameWatcher` | GNOME, KDE, wlroots (via portal) | `NameOwnerChanged` |
//! | `WaylandConnectionHealth` | PortalGeneric (embedded wlroots) | Connection liveness |

use tokio::task::JoinHandle;

use super::HealthReporter;

/// Active compositor health monitoring.
///
/// Implementations subscribe to compositor-specific signals (D-Bus, Wayland
/// connection health) and push HealthEvents when state changes are detected.
pub trait CompositorHealthSource: Send + Sync {
    /// Human-readable name for logging
    fn compositor_name(&self) -> &'static str;

    /// Start monitoring in a background task.
    /// Returns None if no monitoring is possible for this compositor.
    fn start(
        &self,
        reporter: HealthReporter,
        shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<JoinHandle<()>>> + Send + '_>>;
}

/// D-Bus NameOwnerChanged watcher — monitors compositor well-known names.
///
/// Used for GNOME (Mutter names), KDE/wlroots (portal.Desktop name).
pub struct DbusNameWatcher {
    /// D-Bus well-known names to watch
    names: Vec<&'static str>,
    /// Display name for logging
    compositor: &'static str,
}

impl DbusNameWatcher {
    /// GNOME/Mutter compositor health source
    pub fn gnome() -> Self {
        Self {
            names: vec![
                "org.gnome.Mutter.ScreenCast",
                "org.gnome.Mutter.RemoteDesktop",
            ],
            compositor: "GNOME/Mutter",
        }
    }

    /// Portal-based compositor health source (KDE, wlroots via portal)
    pub fn portal() -> Self {
        Self {
            names: vec!["org.freedesktop.portal.Desktop"],
            compositor: "Portal",
        }
    }
}

impl CompositorHealthSource for DbusNameWatcher {
    fn compositor_name(&self) -> &'static str {
        self.compositor
    }

    fn start(
        &self,
        reporter: HealthReporter,
        shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<JoinHandle<()>>> + Send + '_>>
    {
        let names = self.names.clone();
        let compositor = self.compositor;
        Box::pin(async move {
            if names.is_empty() {
                return None;
            }

            let connection = match zbus::Connection::session().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Cannot connect to session bus for {compositor} watching: {e}");
                    return None;
                }
            };

            tracing::info!("Starting {compositor} D-Bus name watcher for: {names:?}");
            Some(tokio::spawn(super::compositor_watcher::watch_dbus_names(
                connection, names, reporter, shutdown,
            )))
        })
    }
}

/// Wayland connection health source for PortalGeneric strategy.
///
/// PortalGeneric has no D-Bus compositor dependency — it embeds the portal
/// backend and talks to the compositor via Wayland directly. Health is
/// determined by whether the embedded portal is still functioning.
///
/// Since the embedded portal runs in-process, connection health is detected
/// via the portal health channel itself (PortalHealthEvent::SessionStateChanged).
/// This source is a no-op placeholder that documents the gap and explains
/// why PortalGeneric doesn't need a D-Bus watcher.
pub struct WaylandConnectionHealth;

impl CompositorHealthSource for WaylandConnectionHealth {
    fn compositor_name(&self) -> &'static str {
        "PortalGeneric (Wayland)"
    }

    fn start(
        &self,
        _reporter: HealthReporter,
        _shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<JoinHandle<()>>> + Send + '_>>
    {
        // PortalGeneric health monitoring is handled by the portal health bridge
        // in portal_generic.rs (PortalHealthEvent::SessionStateChanged → HealthEvent).
        // No separate D-Bus watcher needed — the Wayland connection is in-process.
        Box::pin(async {
            tracing::debug!(
                "PortalGeneric uses in-process health bridge, no D-Bus compositor watcher needed"
            );
            None
        })
    }
}

/// Select the appropriate health source for a session type.
pub fn compositor_health_source(
    session_type: crate::session::SessionType,
) -> Box<dyn CompositorHealthSource> {
    use crate::session::SessionType;

    match session_type {
        SessionType::MutterDirect => Box::new(DbusNameWatcher::gnome()),
        SessionType::Portal
        | SessionType::Libei
        | SessionType::ScreenCastOnly
        | SessionType::WlrDirect
        | SessionType::KwinVirtual => Box::new(DbusNameWatcher::portal()),
        SessionType::PortalGeneric => Box::new(WaylandConnectionHealth),
    }
}
