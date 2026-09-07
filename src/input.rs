//! Input capture (primary, via rdev::listen) and injection (secondary, via rdev::simulate).

use crate::protocol::{InputEvent, MsButton};
use rdev::{simulate, Button as RdevButton, Event, EventType};

/// Start the global input listener. Blocks its own thread running the OS event loop.
/// `cb` is invoked for every global event. (On macOS this requires Accessibility permission.)
pub fn start_capture<F>(cb: F)
where
    F: FnMut(Event) + Send + 'static,
{
    std::thread::spawn(move || {
        if let Err(e) = rdev::listen(cb) {
            log::error!("input capture failed: {:?}", e);
            log::error!("On macOS: grant Accessibility permission to the Terminal/app. On Linux: run under X11.");
            crate::diag::log(&format!("CAPTURE FAILED: {:?} (check Accessibility permission)", e));
        } else {
            crate::diag::log("capture thread started (event tap active)");
        }
    });
}

/// Apply a forwarded input event on this machine (used by secondaries).
pub fn apply_input(ev: &InputEvent) {
    let result = match ev {
        InputEvent::MouseMove { x, y } => simulate(&EventType::MouseMove { x: *x, y: *y }),
        InputEvent::MouseDown { button } => simulate(&EventType::ButtonPress(button.to_rdev())),
        InputEvent::MouseUp { button } => simulate(&EventType::ButtonRelease(button.to_rdev())),
        InputEvent::Wheel { dx, dy } => simulate(&EventType::Wheel { delta_x: *dx, delta_y: *dy }),
        InputEvent::KeyDown { key } => simulate(&EventType::KeyPress(key.clone())),
        InputEvent::KeyUp { key } => simulate(&EventType::KeyRelease(key.clone())),
    };
    if let Err(e) = result {
        log::debug!("inject failed: {:?}", e);
    }
}

/// Warp the local (primary) cursor to an absolute position — the "treadmill" trick that lets
/// the physical cursor keep generating motion past a screen edge so the virtual cursor can
/// continue onto a neighbouring screen.
pub fn warp_cursor(x: f64, y: f64) {
    let _ = simulate(&EventType::MouseMove { x, y });
}

/// Read the cursor position directly from the OS, *not* from the event stream.
///
/// While the OS pins the cursor against a display edge it may deliver no motion events at
/// all (or only zero-delta echoes), which makes a purely event-driven edge-crossing
/// unreliable. The remote-motion driver (src/drive.rs) samples this instead: same Core
/// Graphics global space (`CGEventGetLocation`, origin = top-left of the main display, y
/// down) that rdev reports for motion events, so the coordinates are interchangeable.
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
/// X display (e.g. a Wayland session) — the driver then falls back to the event stream.
///
/// The display connection is cached: the remote-motion driver samples at a high rate, and
/// `XOpenDisplay`/`XCloseDisplay` on every sample (a full socket handshake each time) is
/// expensive enough to starve the poll loop.
#[cfg(target_os = "linux")]
pub fn cursor_position() -> Option<(f64, f64)> {
    use x11_dl::xlib::{Display, Xlib};

    // Cache library handle + display connection for the process lifetime. `XOpenDisplay` is a
    // full socket handshake; doing it on every sample would starve the poll loop.
    // NB: the pointer is kept as `usize` because raw pointers are neither `Send` nor `Sync`
    // and therefore cannot live inside a `static`.
    static CONN: std::sync::OnceLock<Option<(&'static Xlib, usize)>> = std::sync::OnceLock::new();

    let (xlib, display) = CONN
        .get_or_init(|| {
            let xlib: &'static Xlib = Box::leak(Box::new(Xlib::open().ok()?));
            // Xlib is not thread-safe by default and we sample from a dedicated driver thread.
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
            display,
            root,
            &mut root_ret,
            &mut child_ret,
            &mut rx,
            &mut ry,
            &mut wx,
            &mut wy,
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
