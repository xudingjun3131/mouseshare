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
/// unreliable. The edge-rest poller samples this instead: same Core Graphics global space
/// (`CGEventGetLocation`, origin = top-left of the main display, y down) that rdev reports
/// for motion events, so the coordinates are interchangeable.
#[cfg(target_os = "macos")]
pub fn cursor_position() -> Option<(f64, f64)> {
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let ev = CGEvent::new(src).ok()?;
    let p = ev.location();
    Some((p.x, p.y))
}

/// Non-macOS: no direct sampler wired up yet; the event stream is the only source.
#[cfg(not(target_os = "macos"))]
pub fn cursor_position() -> Option<(f64, f64)> {
    None
}

pub fn button_to_ms(b: RdevButton) -> MsButton {
    MsButton::from_rdev(b)
}
