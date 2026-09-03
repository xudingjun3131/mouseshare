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

pub fn button_to_ms(b: RdevButton) -> MsButton {
    MsButton::from_rdev(b)
}
