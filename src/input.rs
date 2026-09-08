//! Input injection (secondary, via rdev::simulate) and cursor helpers shared by the capture layer.
//!
//! ## Why injection is *relative*
//!
//! The protocol forwards `MouseMotion { dx, dy }` — a delta, not a position. To apply it we read
//! the receiver's *own* current cursor position, add the delta, clamp to the receiver's own screen
//! rectangle (stored at startup via [`set_local_layout`]), and inject the resulting absolute
//! position. This keeps the two machines' coordinate spaces completely independent: the primary may
//! be HiDPI and the secondary 1x, yet the cursor tracks 1:1 because only deltas cross the wire.

use crate::layout::Layout;
use crate::protocol::{InputEvent, MsButton};
use rdev::Button as RdevButton;
// `simulate` and `EventType` are only used by the real (non-test) injection paths; under
// `cfg(test)` injection is a no-op so they would be unused.
#[cfg(not(test))]
use rdev::{simulate, EventType};
use std::sync::OnceLock;

/// The receiver's own screens, used to clamp injected cursor positions. Set once at startup on
/// every machine (even primaries, harmlessly) via [`set_local_layout`].
static LOCAL_LAYOUT: OnceLock<Layout> = OnceLock::new();

pub fn set_local_layout(layout: &Layout) {
    let _ = LOCAL_LAYOUT.set(layout.clone());
}

/// Apply a forwarded input event on this machine (used by secondaries).
///
/// `MouseMotion` is applied relatively (see module docs); everything else is forwarded verbatim.
#[cfg(not(test))]
pub fn apply_input(ev: &InputEvent) {
    let result = match ev {
        InputEvent::MouseMotion { dx, dy } => inject_relative(*dx, *dy),
        InputEvent::MouseDown { button } => simulate(&EventType::ButtonPress(button.to_rdev())),
        InputEvent::MouseUp { button } => simulate(&EventType::ButtonRelease(button.to_rdev())),
        InputEvent::Wheel { dx, dy } => simulate(&EventType::Wheel {
            delta_x: *dx,
            delta_y: *dy,
        }),
        InputEvent::KeyDown { key } => simulate(&EventType::KeyPress(key.clone())),
        InputEvent::KeyUp { key } => simulate(&EventType::KeyRelease(key.clone())),
    };
    if let Err(e) = result {
        log::debug!("inject failed: {:?}", e);
    }
}

/// Under test, injection is a no-op so the headless control-plane tests never touch the device.
#[cfg(test)]
pub fn apply_input(_ev: &InputEvent) {}

/// Relative motion: read the current cursor, add the delta, clamp to this machine's own screen,
/// and inject the absolute result.
#[cfg(not(test))]
fn inject_relative(dx: f64, dy: f64) -> Result<(), rdev::SimulateError> {
    let Some((cx, cy)) = cursor_position() else {
        // No OS sampler (shouldn't happen on a real secondary) — fall back to a raw absolute
        // move from the origin so at least *something* happens.
        return simulate(&EventType::MouseMove { x: dx, y: dy });
    };
    let (mut nx, mut ny) = (cx + dx, cy + dy);
    if let Some(layout) = LOCAL_LAYOUT.get() {
        if let Some((bl, bt, br, bb)) = layout.local_bbox() {
            nx = nx.clamp(bl, br - 1.0);
            ny = ny.clamp(bt, bb - 1.0);
        }
    }
    simulate(&EventType::MouseMove { x: nx, y: ny })
}

/// Warp the local cursor to an absolute position — used by the capture layer to keep the (hidden)
/// cursor parked against the shared edge while a secondary has control, so the OS never clamps it.
#[cfg(not(test))]
pub fn warp_cursor(x: f64, y: f64) {
    let _ = simulate(&EventType::MouseMove { x, y });
}

/// Under test, warping is a no-op (headless agent has no cursor to move).
#[cfg(test)]
pub fn warp_cursor(_x: f64, _y: f64) {}

/// Read the cursor position directly from the OS, *not* from the event stream.
///
/// While the OS pins the cursor against a display edge it may deliver no motion events at all,
/// which makes a purely event-driven edge-crossing unreliable. The capture layer samples this
/// instead — same Core Graphics global space (`CGEventGetLocation`, origin = top-left of the main
/// display, y down) that `MOUSE_EVENT_DELTA_*` are measured against, so the coordinates are
/// interchangeable.
#[cfg(target_os = "macos")]
pub fn cursor_position() -> Option<(f64, f64)> {
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let ev = CGEvent::new(src).ok()?;
    let p = ev.location();
    Some((p.x, p.y))
}

/// Windows: `GetCursorPos` in virtual-desktop coordinates. The process is declared
/// per-monitor DPI aware at startup (see main), so these are *physical* pixels — the same
/// space `MOUSEEVENTF_ABSOLUTE` injection normalises against.
#[cfg(target_os = "windows")]
pub fn cursor_position() -> Option<(f64, f64)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(lp_point: *mut Point) -> i32;
    }
    let mut p = Point { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut p) };
    if ok != 0 {
        Some((p.x as f64, p.y as f64))
    } else {
        None
    }
}

/// Linux/X11: `XQueryPointer` on the default root window. Returns `None` when there is no
/// X display (e.g. a Wayland session).
///
/// The display connection is cached: the capture layer samples at a high rate, and
/// `XOpenDisplay`/`XCloseDisplay` on every sample (a full socket handshake each time) is
/// expensive enough to starve the poll loop.
#[cfg(target_os = "linux")]
pub fn cursor_position() -> Option<(f64, f64)> {
    use x11_dl::xlib::{Display, Xlib};

    static CONN: std::sync::OnceLock<Option<(&'static Xlib, usize)>> = std::sync::OnceLock::new();

    let (xlib, display) = CONN
        .get_or_init(|| {
            let xlib: &'static Xlib = Box::leak(Box::new(Xlib::open().ok()?));
            unsafe { (xlib.XInitThreads)() };
            let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
            if display.is_null() {
                return None;
            }
            Some((xlib, display as usize))
        })
        .as_ref()?;
    let (xlib, display) = (*xlib, *display as *mut Display);

    unsafe {
        let root = (xlib.XRootWindow)(display, 0);
        let mut root_ret = 0u64;
        let mut child_ret = 0u64;
        let mut rx = 0i32;
        let mut ry = 0i32;
        let mut wx = 0i32;
        let mut wy = 0i32;
        let mut mask = 0u32;
        let ok = (xlib.XQueryPointer)(
            display, root, &mut root_ret, &mut child_ret, &mut rx, &mut ry, &mut wx, &mut wy,
            &mut mask,
        );
        if ok != 0 {
            Some((rx as f64, ry as f64))
        } else {
            None
        }
    }
}

/// Other platforms: no direct sampler wired up.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub fn cursor_position() -> Option<(f64, f64)> {
    None
}

pub fn button_to_ms(b: RdevButton) -> MsButton {
    MsButton::from_rdev(b)
}
