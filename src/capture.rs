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

use crate::control::{on_capture, CaptureMode, GrabCtx, RawInput};
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
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopSource};
#[cfg(target_os = "macos")]
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};

// ---- cursor visibility / parking (capture-layer concern) ----

/// Magic value stamped into `kCGEventSourceUserData` on every event MouseShare synthesises.
///
/// The event tap ignores anything carrying it. That is what makes it safe to move the real cursor
/// while we are grabbing: without a tag, our own warp would come straight back through the tap and
/// look like a violent user swipe (which is exactly why the old code refused to re-centre the
/// cursor at all, and why control came back with the pointer wherever the OS had parked it).
#[cfg(target_os = "macos")]
pub const SYNTHETIC_MARK: i64 = 0x4D53_4841_5245; // "MSHARE"

// In tests we never touch the real display server (which may be absent in a headless agent or
// CI), so cursor show/hide/park are no-ops. This lets the control-plane integration tests run
// without a GUI session.
#[cfg(test)]
mod cursor {
    pub fn hide_cursor() {}
    pub fn show_cursor() {}
    pub fn park_cursor(_p: (f64, f64)) {}
    pub fn enter_forwarding_grab() {}
    pub fn leave_forwarding_grab(target: (f64, f64)) {
        crate::input::warp_cursor(target.0, target.1);
    }
    pub fn associate_mouse(_connected: bool) {}
}

#[cfg(all(not(test), target_os = "macos"))]
mod cursor {
    use super::SYNTHETIC_MARK;
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Whether we currently hold the cursor hidden. Tracked because `CGDisplayHideCursor` is a
    /// *counter*, not a flag: an unbalanced hide would leave the user with no pointer at all.
    static HIDDEN: AtomicBool = AtomicBool::new(false);

    fn displays() -> Vec<CGDisplay> {
        match CGDisplay::active_displays() {
            Ok(ids) if !ids.is_empty() => ids.into_iter().map(CGDisplay::new).collect(),
            _ => vec![CGDisplay::main()],
        }
    }

    /// Hide the pointer on **every** display.
    ///
    /// Hiding only `CGDisplay::main()` — the obvious implementation, and what this used to do —
    /// leaves the pointer fully visible whenever the cursor sits on a secondary monitor. The shared
    /// edge is usually on the *external* display, so that is exactly where a hand-off begins: the
    /// pointer stayed on screen and, in any moment the tap was busy, it moved as well. That is the
    /// "after minimising, the mouse slides on both screens" report.
    pub fn hide_cursor() {
        if HIDDEN.swap(true, Ordering::SeqCst) {
            return;
        }
        for d in displays() {
            let _ = d.hide_cursor();
        }
    }

    pub fn show_cursor() {
        if !HIDDEN.swap(false, Ordering::SeqCst) {
            return;
        }
        for d in displays() {
            let _ = d.show_cursor();
        }
    }

    /// Detach the hardware mouse from the cursor (or reattach it).
    ///
    /// `CGAssociateMouseAndMouseCursorPosition(false)` tells the window server to stop letting the
    /// physical mouse drive the pointer. Combined with dropping the events, this is what guarantees
    /// the local pointer cannot creep even if our tap is momentarily disabled — App Nap, a
    /// `TapDisabledByTimeout`, the user revoking Input Monitoring, or the window being minimised so
    /// the app is no longer frontmost (which is also when `CGDisplayHideCursor` silently stops
    /// working). Without it, every one of those windows let the local cursor and the remote cursor
    /// track the same hand movement.
    pub fn associate_mouse(connected: bool) {
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(connected);
    }

    /// Warp the real cursor with a **tagged** event, so the tap ignores what it generates.
    pub fn park_cursor(p: (f64, f64)) {
        let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
            return;
        };
        let Ok(ev) = CGEvent::new_mouse_event(
            src,
            CGEventType::MouseMoved,
            CGPoint::new(p.0, p.1),
            CGMouseButton::Left,
        ) else {
            return;
        };
        ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, SYNTHETIC_MARK);
        ev.post(CGEventTapLocation::HID);
    }

    pub fn enter_forwarding_grab() {
        associate_mouse(false);
        hide_cursor();
    }

    pub fn leave_forwarding_grab(target: (f64, f64)) {
        associate_mouse(true);
        show_cursor();
        park_cursor(target);
    }
}

/// Raise the current thread to `QOS_CLASS_USER_INTERACTIVE`.
///
/// The capture callback, the input pump and the socket writer all sit on the latency path of every
/// mouse event. Left at the default class they get scheduled behind whatever else the machine is
/// doing, which shows up as an uneven, "stuttering" remote cursor — worst right after the window is
/// hidden and the process is treated as less interesting. macOS lets a thread declare that it is on
/// a user-interactive path; doing so costs nothing and removes a whole class of jitter.
#[cfg(all(not(test), target_os = "macos"))]
pub fn boost_current_thread() {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

#[cfg(not(all(not(test), target_os = "macos")))]
pub fn boost_current_thread() {}

#[cfg(all(not(test), not(target_os = "macos")))]
mod cursor {
    pub fn hide_cursor() {}
    pub fn show_cursor() {}
    pub fn park_cursor(_p: (f64, f64)) {}
    /// Windows / X11 have no equivalent of detaching the hardware mouse from the pointer, and the
    /// primary there is still an observer rather than a grab, so this is a no-op.
    pub fn associate_mouse(_connected: bool) {}
    pub fn enter_forwarding_grab() {}
    pub fn leave_forwarding_grab(target: (f64, f64)) {
        crate::input::warp_cursor(target.0, target.1);
    }
}

pub use cursor::{
    associate_mouse, enter_forwarding_grab, hide_cursor, leave_forwarding_grab, park_cursor,
    show_cursor,
};

// ---- macOS: keep App Nap from throttling us while the window is hidden ----

/// Keeps the token returned by `beginActivityWithOptions:reason:` alive. Dropping it (or letting
/// it be released) ends the activity and App Nap switches straight back on.
#[cfg(target_os = "macos")]
static APP_NAP_TOKEN: OnceLock<usize> = OnceLock::new();

/// Opt this process out of macOS App Nap.
///
/// **Why this exists — it is the hidden cause of two separate user-visible bugs:**
///
/// * *"it gets laggy once I minimise/hide the window"*, and
/// * *"both cursors move at once"*.
///
/// When the app's window is not visible macOS treats the process as idle and puts it into
/// **App Nap**: it coalesces timers, throttles scheduling, and — critically — declares the
/// event tap inactive. The tap then receives `TapDisabledByTimeout` and stops running our
/// callback. While the tap is disabled every mouse event goes straight to the system, so the
/// local cursor moves freely; the moment the tap re-arms we are still in `Forwarding` mode and
/// start dropping + forwarding again. To the user that reads as stuttering *and* as two
/// cursors tracking the same hand movement.
///
/// `NSProcessInfo.beginActivityWithOptions:reason:` marks the process user-initiated and
/// latency-critical, which opts it out of App Nap for as long as we hold the returned token.
#[cfg(target_os = "macos")]
pub fn disable_app_nap() {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel};
    use std::ffi::CString;

    // NSActivityOptions (Foundation):
    //   NSActivityIdleDisplaySleepDisabled     = 1 << 40
    //   NSActivityIdleSystemSleepDisabled      = 1 << 20
    //   NSActivitySuddenTerminationDisabled    = 1 << 14
    //   NSActivityAutomaticTerminationDisabled = 1 << 15
    //   NSActivityUserInitiated                = 0x00FFFFFF | NSActivityIdleSystemSleepDisabled
    //   NSActivityLatencyCritical              = 0xFF00000000
    const NS_ACTIVITY_USER_INITIATED: u64 = 0x00FF_FFFF;
    const NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED: u64 = 1 << 20;
    const NS_ACTIVITY_LATENCY_CRITICAL: u64 = 0xFF00_0000_00;

    let options = NS_ACTIVITY_USER_INITIATED
        | NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED
        | NS_ACTIVITY_LATENCY_CRITICAL;

    // NOTE: deliberately **not** wrapped in an `autoreleasepool`. `beginActivity...` returns an
    // *autoreleased* token (it is not an alloc/new/copy selector), so draining a pool around this
    // call releases it — and with it the whole activity. That is precisely the bug this function
    // shipped with: the token pointer we stashed was dangling, App Nap switched straight back on,
    // and hiding/minimising the window throttled the event tap again (lag, no crossing, two
    // cursors). Retaining it and never releasing keeps the activity alive for the process lifetime.
    unsafe {
        let pi: *mut Object = msg_send![class!(NSProcessInfo), processInfo];
        if pi.is_null() {
            return;
        }
        let c = match CString::new("MouseShare forwards input and clipboard in real time") {
            Ok(c) => c,
            Err(_) => return,
        };
        let reason: *mut Object = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
        if reason.is_null() {
            return;
        }
        let token: *mut Object = msg_send![pi, beginActivityWithOptions: options reason: reason];
        if token.is_null() {
            log::warn!("could not begin App Nap activity; hiding the window may cause lag");
            return;
        }
        // +1 our own reference so the (autoreleased) pool drain cannot end the activity.
        let _: *mut Object = msg_send![token, retain];
        let _ = APP_NAP_TOKEN.set(token as usize);
        log::info!("App Nap disabled (latency-critical activity begun)");
        crate::diag::log("app nap disabled");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn disable_app_nap() {}

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
        // The callback runs inside the OS input pipeline: keep this thread at user-interactive
        // QoS so a busy machine cannot schedule it late (an uneven remote cursor).
        boost_current_thread();
        // Raw mach port pointer, used by the callback to re-enable the tap after a timeout.
        let tap_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
        let tap_port_cb = Arc::clone(&tap_port);

        // NOTE: TapDisabledByTimeout / TapDisabledByUserInput must NOT go in this list.
        // The mask bit is `1u64 << event_type`, and those two pseudo-events have values
        // 0xFFFFFFFF / 0xFFFFFFFE — shifting by them overflows: debug builds panic (the
        // capture thread dies instantly, so a dev build never captures), release builds
        // silently wrap and corrupt the mask. macOS always delivers tap-disabled events
        // to the callback regardless of the mask, so they need no entry here either.
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
            // Anything we synthesised ourselves (warping the cursor back to the shared edge on a
            // return) is not user input: pass it straight through without feeding it to the control
            // plane, where a warp would look like an enormous swipe.
            if cg_ev.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == SYNTHETIC_MARK {
                return CallbackResult::Keep;
            }
            // Re-enable a tap the OS disabled (the callback ran too long, or App Nap kicked in).
            if event_type as u32 == CGEventType::TapDisabledByTimeout as u32 {
                // `try_lock`, never `lock`: this callback is already being told it took too long,
                // so the last thing it may do is wait on a mutex. If the GUI thread holds `mode`
                // we simply skip this recovery — the next timeout, or the user moving back,
                // rights it — rather than stalling and earning another disable.
                let fwd = ctx_cb
                    .mode
                    .try_lock()
                    .map(|m| matches!(&*m, CaptureMode::Forwarding(_)))
                    .unwrap_or(false);
                if fwd {
                    crate::diag::log("TAP DISABLED mid-forward — control returned to local");
                    crate::control::try_return_control(&ctx_cb);
                }
                if let Some(&port) = tap_port_cb.get() {
                    unsafe { CGEventTapEnable(port as *mut c_void, true) };
                }
                return CallbackResult::Keep;
            }
            // User revoked input monitoring / secure input: stop dropping so we don't eat input.
            // Same state hazard as a timeout-disable: while the tap is down our Drop verdicts are
            // ignored, so an un-recovered Forwarding state means BOTH cursors keep moving. Hand
            // control back (try_lock — never wait here) and show the cursor.
            if event_type as u32 == CGEventType::TapDisabledByUserInput as u32 {
                let fwd = ctx_cb
                    .mode
                    .try_lock()
                    .map(|m| matches!(&*m, CaptureMode::Forwarding(_)))
                    .unwrap_or(false);
                if fwd {
                    crate::diag::log("TAP DISABLED BY USER INPUT mid-forward — control returned");
                    crate::control::try_return_control(&ctx_cb);
                }
                // Whether or not we were forwarding, the grab is over: reattach the hardware mouse
                // and give the user their pointer back.
                crate::capture::associate_mouse(true);
                crate::capture::show_cursor();
                return CallbackResult::Keep;
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
                // Modifiers do NOT arrive as KeyDown/KeyUp on macOS — only as FlagsChanged.
                // Without reading them here, the Ctrl+Alt+Space hotkey never fires: the tap
                // never sees ControlLeft/Alt key events, so the modifier state stays false
                // forever and Space is treated as a plain space. Read the event flags instead.
                CGEventType::FlagsChanged => {
                    let f = cg_ev.get_flags();
                    Some(RawInput::Mods {
                        ctrl: f.contains(CGEventFlags::CGEventFlagControl),
                        alt: f.contains(CGEventFlags::CGEventFlagAlternate),
                    })
                }
                // FlagsChanged (modifier state) and everything else: pass through unchanged.
                _ => None,
            };

            match raw {
                None => CallbackResult::Keep,
                Some(r) => {
                    let drop = on_capture(&ctx_cb, r, location);
                    if drop {
                        CallbackResult::Drop
                    } else {
                        CallbackResult::Keep
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
        let _ = tap_port.set(tap.mach_port().as_concrete_TypeRef() as usize);

        let loop_source: CFRunLoopSource = match tap.mach_port().create_runloop_source(0) {
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
        boost_current_thread();
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
                rdev::EventType::Wheel { delta_x, delta_y } => Some((
                    RawInput::Wheel {
                        dx: delta_x,
                        dy: delta_y,
                    },
                    None,
                )),
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
