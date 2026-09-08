//! Input capture.
//!
//! **macOS** installs a `CGEventTap` (Session + HeadInsertEventTap) that *grabs* input: the callback
//! reads the hardware motion delta (`kCGMouseEventDeltaX/Y`), predicts edge crossings from
//! `location + delta`, and — while a secondary has control — returns `None` so the event is dropped
//! (the OS never receives it and therefore never clamps the cursor at a display edge). This is the
//! root-cause fix for cross-screen; the old `rdev::listen` observer could not do this, which is why
//! the previous build needed treadmill / edge-rest / park-anchor band-aids.
//!
//! **Other platforms** currently fall back to `rdev::listen` (an observer). Their native grab rewrite
//! is deferred; cross-screen there still needs the same treatment (see crate memory).

use crate::control::{on_capture, GrabCtx, RawInput};
use crate::protocol::MsButton;
use rdev::Key;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// True while a capture thread has been started and is (or was) running. Guards against a second
/// grab thread racing the first when the user clicks "re-check" before the previous attempt exits.
static CAPTURE_RUNNING: AtomicBool = AtomicBool::new(false);

// `core-foundation` / `core-graphics` are macOS-only dependencies (declared under
// `[target.'cfg(target_os = "macos")'.dependencies]`). Importing them unconditionally breaks the
// Windows / Linux builds, so they are gated here.
#[cfg(target_os = "macos")]
use core_foundation::base::TCFType;
#[cfg(target_os = "macos")]
use core_foundation::runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
#[cfg(target_os = "macos")]
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};

// ---- cursor visibility / parking (capture-layer concern) ----

// In tests we never touch the real display server (which may be absent in a headless agent or
// CI), so cursor show/hide/park are no-ops. This lets the control-plane integration tests run
// without a GUI session.
#[cfg(test)]
mod cursor {
    pub fn hide_cursor() {}
    pub fn show_cursor() {}
    pub fn park_cursor(_p: (f64, f64)) {}
}

#[cfg(all(not(test), target_os = "macos"))]
mod cursor {
    use core_graphics::display::CGDisplay;
    pub fn hide_cursor() {
        let _ = CGDisplay::hide_cursor(&CGDisplay::main());
    }
    pub fn show_cursor() {
        let _ = CGDisplay::show_cursor(&CGDisplay::main());
    }
    /// With a grab tap the cursor is frozen by dropping events, so nothing needs to be warped.
    pub fn park_cursor(_p: (f64, f64)) {}
}

#[cfg(all(not(test), not(target_os = "macos")))]
mod cursor {
    pub fn hide_cursor() {}
    pub fn show_cursor() {}
    pub fn park_cursor(_p: (f64, f64)) {}
}

pub use cursor::{hide_cursor, park_cursor, show_cursor};

/// Start the global input capture. Blocks its own thread running the OS event loop.
///
/// `failed` is a shared flag the UI polls: it is set when the capture layer cannot obtain the
/// required OS permission (so the GUI can pop the native permission prompt + guidance dialog),
/// and cleared again each time capture is (re)started.
pub fn start_capture(ctx: Arc<GrabCtx>, failed: Arc<AtomicBool>) {
    // Idempotent: only one grab thread may exist. If the previous attempt is still winding down the
    // user clicked "re-check" too fast — drop this request rather than spawn a second event tap.
    if CAPTURE_RUNNING.swap(true, Ordering::SeqCst) {
        log::warn!("input capture already running; ignoring duplicate start");
        return;
    }
    failed.store(false, Ordering::SeqCst);
    #[cfg(target_os = "macos")]
    {
        start_capture_macos(ctx, failed);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = failed;
        start_capture_observer(ctx);
    }
}

// ---- macOS: permission prompting (Accessibility + Input Monitoring) ----

/// Ask macOS to present the *native* permission dialogs for the two entitlements MouseShare needs:
/// **Input Monitoring** (listening for the event tap, macOS 10.15+) and **Accessibility** (posting
/// synthesized events, both roles). Calling these is what makes the system prompt appear; without
/// them macOS stays silent and the tap just fails. Each prompt shows at most once per install — if
/// the user previously dismissed it they must grant access in System Settings manually, which the
/// GUI's guidance dialog links to.
#[cfg(target_os = "macos")]
pub fn trigger_permission_prompts() {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        // `kAXTrustedCheckOptionPrompt = true` makes this present the "allow accessibility" sheet.
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }

    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    unsafe {
        // Input Monitoring: only prompt if not already granted (prompting again is a no-op anyway,
        // but avoid the pointless call when we're already trusted).
        if !CGPreflightListenEventAccess() {
            let _ = CGRequestListenEventAccess();
        }
        if !CGPreflightPostEventAccess() {
            let _ = CGRequestPostEventAccess();
        }
        // Accessibility: prompt (kAXTrustedCheckOptionPrompt = true) so the user can approve.
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = CFBoolean::true_value();
        let dict = CFDictionary::<CFString, CFBoolean>::from_CFType_pairs(&[(key, value)]);
        let _ = AXIsProcessTrustedWithOptions(dict.as_CFTypeRef());
    }
}

#[cfg(not(target_os = "macos"))]
pub fn trigger_permission_prompts() {}

// ---- macOS: native event-tap grab ----

#[cfg(target_os = "macos")]
fn start_capture_macos(ctx: Arc<GrabCtx>, failed: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // Raw mach port pointer, used by the callback to re-enable the tap after a timeout.
        let tap_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
        let tap_port_cb = Arc::clone(&tap_port);

        let events_of_interest: Vec<CGEventType> = vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::TapDisabledByTimeout,
            CGEventType::TapDisabledByUserInput,
        ];

        let ctx_cb = Arc::clone(&ctx);
        let callback = move |_proxy: core_graphics::event::CGEventTapProxy,
                             event_type: CGEventType,
                             cg_ev: &CGEvent| {
            // Re-enable a tap the OS disabled for running too long in one callback (load / App Nap).
            if event_type as u32 == CGEventType::TapDisabledByTimeout as u32 {
                if let Some(&port) = tap_port_cb.get() {
                    unsafe { CGEventTapEnable(port as *mut c_void, true) };
                }
                return None;
            }
            // User revoked input monitoring / secure input: stop dropping so we don't eat input.
            if event_type as u32 == CGEventType::TapDisabledByUserInput as u32 {
                crate::capture::show_cursor();
                return None;
            }

            let loc = cg_ev.location();
            let location = Some((loc.x, loc.y));

            let raw = match event_type {
                CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged => {
                    let dx = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
                    let dy = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
                    Some(RawInput::Motion { dx, dy })
                }
                CGEventType::LeftMouseDown => Some(RawInput::ButtonDown(MsButton::Left)),
                CGEventType::LeftMouseUp => Some(RawInput::ButtonUp(MsButton::Left)),
                CGEventType::RightMouseDown => Some(RawInput::ButtonDown(MsButton::Right)),
                CGEventType::RightMouseUp => Some(RawInput::ButtonUp(MsButton::Right)),
                CGEventType::OtherMouseDown => {
                    let n = cg_ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
                    Some(RawInput::ButtonDown(mouse_button_from_num(n)))
                }
                CGEventType::OtherMouseUp => {
                    let n = cg_ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
                    Some(RawInput::ButtonUp(mouse_button_from_num(n)))
                }
                CGEventType::ScrollWheel => {
                    let v = cg_ev
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
                    let h = cg_ev
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
                    Some(RawInput::Wheel { dx: h, dy: v })
                }
                CGEventType::KeyDown => Some(RawInput::KeyDown(key_from_code(
                    cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16,
                ))),
                CGEventType::KeyUp => Some(RawInput::KeyUp(key_from_code(
                    cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16,
                ))),
                // FlagsChanged (modifier state) and everything else: pass through unchanged.
                _ => None,
            };

            match raw {
                None => Some(cg_ev.clone()),
                Some(r) => {
                    let drop = on_capture(&ctx_cb, r, location);
                    if drop {
                        None
                    } else {
                        Some(cg_ev.clone())
                    }
                }
            }
        };

        let tap = match CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            events_of_interest,
            callback,
        ) {
            Ok(t) => t,
            Err(_) => {
                log::error!(
                    "CGEventTap creation failed — grant Accessibility + Input Monitoring permission"
                );
                crate::diag::log(
                    "CAPTURE FAILED: could not create CGEventTap (check Accessibility / Input Monitoring permission)",
                );
                // Surface it to the UI (guidance dialog) and re-arm the guard so a retry can run.
                // The native permission prompts themselves are triggered by the UI thread, which
                // is the reliable place for macOS to present the dialog.
                failed.store(true, Ordering::SeqCst);
                CAPTURE_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
        };

        // Stash the raw mach port so the callback can re-enable the tap on timeout.
        let _ = tap_port.set(tap.mach_port.as_concrete_TypeRef() as usize);

        let loop_source: CFRunLoopSource = match tap.mach_port.create_runloop_source(0) {
            Ok(s) => s,
            Err(_) => {
                log::error!("failed to create runloop source for event tap");
                return;
            }
        };
        unsafe {
            CFRunLoop::get_current().add_source(&loop_source, kCFRunLoopCommonModes);
        }
        tap.enable();
        log::info!("macOS event tap active (grab capture)");
        crate::diag::log("capture thread started (CGEventTap grab active)");
        CFRunLoop::run_current();
    });
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

#[cfg(target_os = "macos")]
fn mouse_button_from_num(n: i64) -> MsButton {
    match n {
        0 => MsButton::Left,
        1 => MsButton::Right,
        2 => MsButton::Middle,
        other => MsButton::Other(other as u8),
    }
}

/// Map a macOS `CGKeyCode` to an `rdev::Key`. Copied from rdev's own `key_from_code` (its platform
/// module is private) so we can forward keys through the tap with the same mapping.
#[cfg(target_os = "macos")]
fn key_from_code(code: u16) -> Key {
    match code {
        58 => Key::Alt,
        61 => Key::AltGr,
        51 => Key::Backspace,
        57 => Key::CapsLock,
        59 => Key::ControlLeft,
        62 => Key::ControlRight,
        125 => Key::DownArrow,
        53 => Key::Escape,
        122 => Key::F1,
        109 => Key::F10,
        103 => Key::F11,
        111 => Key::F12,
        120 => Key::F2,
        99 => Key::F3,
        118 => Key::F4,
        96 => Key::F5,
        97 => Key::F6,
        98 => Key::F7,
        100 => Key::F8,
        101 => Key::F9,
        123 => Key::LeftArrow,
        55 => Key::MetaLeft,
        54 => Key::MetaRight,
        36 => Key::Return,
        124 => Key::RightArrow,
        56 => Key::ShiftLeft,
        60 => Key::ShiftRight,
        49 => Key::Space,
        48 => Key::Tab,
        126 => Key::UpArrow,
        50 => Key::BackQuote,
        18 => Key::Num1,
        19 => Key::Num2,
        20 => Key::Num3,
        21 => Key::Num4,
        23 => Key::Num5,
        22 => Key::Num6,
        26 => Key::Num7,
        28 => Key::Num8,
        25 => Key::Num9,
        29 => Key::Num0,
        27 => Key::Minus,
        24 => Key::Equal,
        12 => Key::KeyQ,
        13 => Key::KeyW,
        14 => Key::KeyE,
        15 => Key::KeyR,
        17 => Key::KeyT,
        16 => Key::KeyY,
        32 => Key::KeyU,
        34 => Key::KeyI,
        31 => Key::KeyO,
        35 => Key::KeyP,
        33 => Key::LeftBracket,
        30 => Key::RightBracket,
        0 => Key::KeyA,
        1 => Key::KeyS,
        2 => Key::KeyD,
        3 => Key::KeyF,
        5 => Key::KeyG,
        4 => Key::KeyH,
        38 => Key::KeyJ,
        40 => Key::KeyK,
        37 => Key::KeyL,
        41 => Key::SemiColon,
        39 => Key::Quote,
        42 => Key::BackSlash,
        6 => Key::KeyZ,
        7 => Key::KeyX,
        8 => Key::KeyC,
        9 => Key::KeyV,
        11 => Key::KeyB,
        45 => Key::KeyN,
        46 => Key::KeyM,
        43 => Key::Comma,
        47 => Key::Dot,
        44 => Key::Slash,
        63 => Key::Function,
        other => Key::Unknown(other as u32),
    }
}

// ---- Other platforms: rdev observer (legacy, deferred grab rewrite) ----

#[cfg(not(target_os = "macos"))]
fn start_capture_observer(ctx: Arc<GrabCtx>) {
    use std::sync::Mutex;
    static LAST: OnceLock<Mutex<(f64, f64)>> = OnceLock::new();
    std::thread::spawn(move || {
        if let Err(e) = rdev::listen(move |event: rdev::Event| {
            let raw_and_loc = match event.event_type {
                rdev::EventType::MouseMove { x, y } => {
                    let mut last = LAST.get_or_init(|| Mutex::new((x, y))).lock().unwrap();
                    let d = (x - last.0, y - last.1);
                    *last = (x, y);
                    Some((RawInput::Motion { dx: d.0, dy: d.1 }, Some((x, y))))
                }
                rdev::EventType::ButtonPress(b) => {
                    Some((RawInput::ButtonDown(crate::input::button_to_ms(b)), None))
                }
                rdev::EventType::ButtonRelease(b) => {
                    Some((RawInput::ButtonUp(crate::input::button_to_ms(b)), None))
                }
                rdev::EventType::Wheel { delta_x, delta_y } => {
                    Some((RawInput::Wheel { dx: delta_x, dy: delta_y }, None))
                }
                rdev::EventType::KeyPress(k) => Some((RawInput::KeyDown(k), None)),
                rdev::EventType::KeyRelease(k) => Some((RawInput::KeyUp(k), None)),
                _ => None,
            };
            if let Some((raw, location)) = raw_and_loc {
                // The rdev observer cannot drop events, so the returned `drop` flag is ignored here
                // (cross-screen on this platform is still the old, imperfect behaviour). It keeps the
                // binary building and the local cursor working until the native grab is ported.
                let _drop = on_capture(&ctx, raw, location);
            }
        }) {
            log::error!("input capture failed: {:?}", e);
            crate::diag::log(&format!(
                "CAPTURE FAILED: {:?} (check Accessibility permission)",
                e
            ));
        } else {
            crate::diag::log("capture thread started (rdev observer — legacy, not yet grab)");
        }
    });
}
