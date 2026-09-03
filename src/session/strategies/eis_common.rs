//! Shared EIS (Emulated Input Server) utilities
//!
//! Common types and functions used by both `mutter_direct` and `libei` strategies
//! for device tracking, input event injection, and EIS protocol helpers.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use reis::ei;
use tokio::sync::{Mutex, RwLock};

/// Region defining the coordinate space for an absolute pointer device.
/// Provided by the compositor via EIS Device::Region events.
#[derive(Debug, Clone)]
pub struct DeviceRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Tracked data for an EIS device (keyboard, pointer, touchscreen, etc.)
#[derive(Default)]
pub struct DeviceData {
    pub name: Option<String>,
    pub device_type: Option<ei::device::DeviceType>,
    pub interfaces: HashMap<String, reis::Object>,
    pub seat: Option<ei::Seat>,
    /// Regions for absolute pointer devices (defines coordinate space)
    pub regions: Vec<DeviceRegion>,
}

impl DeviceData {
    /// Downcast a stored interface object to its typed form.
    pub fn interface<T: reis::Interface>(&self) -> Option<T> {
        self.interfaces.get(T::NAME)?.clone().downcast()
    }
}

/// Tracked devices for an EIS session, separated by role.
pub struct EisDevices {
    pub all: Mutex<HashMap<ei::Device, DeviceData>>,
    pub keyboard: Mutex<Option<ei::Device>>,
    /// Device with ei_pointer (relative motion)
    pub pointer: Mutex<Option<ei::Device>>,
    /// Device with ei_pointer_absolute (absolute coordinates).
    /// KDE creates separate devices for relative and absolute pointers.
    pub pointer_absolute: Mutex<Option<ei::Device>>,
    pub touch: Mutex<Option<ei::Device>>,
    pub last_serial: Mutex<u32>,
}

impl EisDevices {
    pub fn new(initial_serial: u32) -> Self {
        Self {
            all: Mutex::new(HashMap::new()),
            keyboard: Mutex::new(None),
            pointer: Mutex::new(None),
            pointer_absolute: Mutex::new(None),
            touch: Mutex::new(None),
            last_serial: Mutex::new(initial_serial),
        }
    }

    /// Clear all device state for EIS session recovery.
    pub async fn clear(&self) {
        self.all.lock().await.clear();
        *self.keyboard.lock().await = None;
        *self.pointer.lock().await = None;
        *self.pointer_absolute.lock().await = None;
        *self.touch.lock().await = None;
    }
}

/// Current time in microseconds (for EIS frame timestamps).
pub fn current_time_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Helper: get device + interface + serial, flush frame after closure.
/// Helper: get device + interface + serial, flush frame after closure.
///
/// Accepts `RwLock<Option<ei::Context>>` for the libei strategy (deferred EIS
/// activation) and `RwLock<ei::Context>` for mutter_direct (always active).
/// Both work because the mutter_direct context is wrapped in Option at call
/// sites via `.as_ref()`.
async fn with_device_interface<T: reis::Interface>(
    context: &RwLock<Option<ei::Context>>,
    device_lock: &Mutex<Option<ei::Device>>,
    devices: &EisDevices,
    device_name: &str,
    f: impl FnOnce(&T),
) -> Result<()> {
    let device = device_lock
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow!("EIS {device_name} not ready"))?;

    let devs = devices.all.lock().await;
    let data = devs
        .get(&device)
        .ok_or_else(|| anyhow!("Device data missing for {device_name}"))?;
    let iface = data
        .interface::<T>()
        .ok_or_else(|| anyhow!("{} interface not found", std::any::type_name::<T>()))?;
    drop(devs);

    f(&iface);

    let serial = *devices.last_serial.lock().await;
    device.frame(serial, current_time_us());
    let ctx = context.read().await;
    let ctx_ref = ctx
        .as_ref()
        .ok_or_else(|| anyhow!("EIS context not initialized"))?;
    ctx_ref.flush()?;
    Ok(())
}

// === Keyboard ===

pub async fn eis_keyboard_keycode(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    keycode: i32,
    pressed: bool,
) -> Result<()> {
    // EIS Keyboard::key() takes Linux evdev keycodes directly.
    // The input handler already converts RDP scancodes to evdev via lamco-rdp-input.
    let eis_keycode = keycode as u32;
    let state = if pressed {
        ei::keyboard::KeyState::Press
    } else {
        ei::keyboard::KeyState::Released
    };

    with_device_interface::<ei::Keyboard>(context, &devices.keyboard, devices, "keyboard", |kbd| {
        kbd.key(eis_keycode, state);
    })
    .await
}

// === Pointer (absolute) ===

pub async fn eis_pointer_motion_absolute(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    x: f64,
    y: f64,
) -> Result<()> {
    // Never apply an EIS region offset: the absolute pointer coordinates
    // arriving here are already in GLOBAL compositor space. The input
    // handler's CoordinateTransformer maps RDP desktop coords into the
    // captured monitor's geometry INCLUDING its position
    // (StreamInfo.position feeds the transformer's monitor layout — e.g.
    // a virtual output at (1920,0) yields global coords in [1920, 3840)).
    // Adding an EIS region offset on top double-offsets the event and
    // lands clicks outside the desktop, while the client-rendered cursor
    // looks fine. KWin takes motion_absolute as global coordinates
    // directly (PointerInputRedirection::processMotionAbsolute); no
    // region translation is needed or correct.

    with_device_interface::<ei::PointerAbsolute>(
        context,
        &devices.pointer_absolute,
        devices,
        "pointer_absolute",
        |ptr| {
            ptr.motion_absolute(x as f32, y as f32);
        },
    )
    .await
}

// === Pointer (relative) ===

pub async fn eis_pointer_motion_relative(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    dx: f64,
    dy: f64,
) -> Result<()> {
    with_device_interface::<ei::Pointer>(context, &devices.pointer, devices, "pointer", |ptr| {
        ptr.motion_relative(dx as f32, dy as f32);
    })
    .await
}

// === Button ===

pub async fn eis_pointer_button(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    button: i32,
    pressed: bool,
) -> Result<()> {
    with_device_interface::<ei::Button>(context, &devices.pointer, devices, "pointer", |btn| {
        btn.button(
            button as u32,
            if pressed {
                ei::button::ButtonState::Press
            } else {
                ei::button::ButtonState::Released
            },
        );
    })
    .await
}

// === Scroll ===

pub async fn eis_pointer_axis(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    dx: f64,
    dy: f64,
) -> Result<()> {
    with_device_interface::<ei::Scroll>(context, &devices.pointer, devices, "pointer", |scroll| {
        if dx.abs() > 0.01 {
            scroll.scroll(dx as f32, 0.0);
        }
        if dy.abs() > 0.01 {
            scroll.scroll(0.0, dy as f32);
        }
    })
    .await
}

// === Touch ===

pub async fn eis_touch_down(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    slot: u32,
    x: f64,
    y: f64,
) -> Result<()> {
    with_device_interface::<ei::Touchscreen>(
        context,
        &devices.touch,
        devices,
        "touchscreen",
        |ts| {
            ts.down(slot, x as f32, y as f32);
        },
    )
    .await
}

pub async fn eis_touch_motion(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    slot: u32,
    x: f64,
    y: f64,
) -> Result<()> {
    with_device_interface::<ei::Touchscreen>(
        context,
        &devices.touch,
        devices,
        "touchscreen",
        |ts| {
            ts.motion(slot, x as f32, y as f32);
        },
    )
    .await
}

pub async fn eis_touch_up(
    context: &RwLock<Option<ei::Context>>,
    devices: &EisDevices,
    slot: u32,
) -> Result<()> {
    with_device_interface::<ei::Touchscreen>(
        context,
        &devices.touch,
        devices,
        "touchscreen",
        |ts| {
            ts.up(slot);
        },
    )
    .await
}

/// Process a device Done event and assign tracked device roles.
///
/// Call this during device discovery when `ei::device::Event::Done` fires.
/// Returns `true` if the device was a virtual device (sender) and was assigned.
pub async fn assign_device_roles(
    device: &ei::Device,
    data: &DeviceData,
    devices: &EisDevices,
) -> bool {
    if !matches!(data.device_type, Some(ei::device::DeviceType::Virtual)) {
        return false;
    }

    if data.interface::<ei::Keyboard>().is_some() {
        *devices.keyboard.lock().await = Some(device.clone());
        tracing::info!("[eis] Keyboard device ready");
    }

    if data.interface::<ei::PointerAbsolute>().is_some() {
        *devices.pointer_absolute.lock().await = Some(device.clone());
        tracing::info!("[eis] Pointer absolute device ready");
    }
    if data.interface::<ei::Pointer>().is_some() {
        *devices.pointer.lock().await = Some(device.clone());
        tracing::info!("[eis] Pointer relative device ready");
    }

    if data.interface::<ei::Touchscreen>().is_some() {
        *devices.touch.lock().await = Some(device.clone());
        tracing::info!("[eis] Touchscreen device ready");
    }

    true
}
