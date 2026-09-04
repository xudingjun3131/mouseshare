//! Delta event tap (macOS): raw mouse-motion deltas straight from the hardware.
//!
//! While a secondary has control, the primary must translate the user's physical mouse
//! motion into remote cursor deltas. The event stream (rdev::listen) is the wrong source
//! for that: it reports absolute cursor *positions*, which are clamped by the OS the moment
//! the cursor touches a display edge. After a hand-off the cursor sits pinned at the shared
//! edge, so every position looks like a huge (edge − park) jump, and while the user keeps
//! pushing into the edge the positions never change at all — the remote cursor starves.
//!
//! Mouse *deltas* (`kCGMouseEventDeltaX/Y`) are computed from the hardware before any
//! clamping, so they keep flowing no matter where the cursor is pinned. This module runs a
//! listen-only `CGEventTap` for mouse-moved/dragged events on its own thread and hands the
//! deltas to a callback. Nothing is warped during remote control any more: the local cursor
//! is hidden and may roam/pin freely — irrelevant, since RETURN places it explicitly.

#![allow(non_snake_case, improper_ctypes_definitions, non_upper_case_globals)]

use std::os::raw::c_void;

type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFAllocatorRef = *mut c_void;
type CFIndex = i64;
type CFRunLoopMode = *const c_void;

// CGEventTapLocation: kCGHIDEventTap = 0 (same tap level rdev's listen uses).
const K_CG_HID_EVENT_TAP: u32 = 0;
// CGEventTapPlacement: kCGHeadInsertEventTap = 0.
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
// CGEventTapOption: kCGEventTapOptionListenOnly = 1 (never swallow events).
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
// CGEventType: MouseMoved = 5, LeftMouseDragged = 6, RightMouseDragged = 7.
const MASK_MOUSE_MOTION: u64 = (1 << 5) | (1 << 6) | (1 << 7);
// CGEventField: kCGMouseEventDeltaX = 4, kCGMouseEventDeltaY = 5.
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        eventsOfInterest: u64,
        callback: QCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetDoubleValueField(event: *mut c_void, field: u32) -> f64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        tap: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
    fn CFRunLoopRun();
    static kCFRunLoopCommonModes: CFRunLoopMode;
}

type QCallback = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    etype: u32,
    event: *mut c_void,
    user_info: *mut c_void,
) -> *mut c_void;

static mut CALLBACK: Option<Box<dyn FnMut(f64, f64) + Send>> = None;

/// The tap callback runs on the tap's run-loop thread. Deltas arrive here even while the
/// cursor is clamped at a display edge (positions would be frozen; deltas are not).
unsafe extern "C" fn delta_callback(
    _proxy: CGEventTapProxy,
    _etype: u32,
    event: *mut c_void,
    _user_info: *mut c_void,
) -> *mut c_void {
    let dx = CGEventGetDoubleValueField(event, FIELD_DELTA_X);
    let dy = CGEventGetDoubleValueField(event, FIELD_DELTA_Y);
    if dx != 0.0 || dy != 0.0 {
        if let Some(cb) = &mut CALLBACK {
            cb(dx, dy);
        }
    }
    event
}

/// Start the delta tap on a dedicated thread (tap creation and the run loop must live on
/// the same thread). Returns `false` if the tap could not be created (missing Accessibility
/// permission — the same permission rdev's listener needs).
pub fn start(cb: Box<dyn FnMut(f64, f64) + Send>) -> bool {
    unsafe {
        CALLBACK = Some(cb);
        std::thread::spawn(|| unsafe {
            let tap = CGEventTapCreate(
                K_CG_HID_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                MASK_MOUSE_MOTION,
                delta_callback,
                std::ptr::null_mut(),
            );
            if tap.is_null() {
                crate::diag::log("delta tap: CGEventTapCreate failed (Accessibility?)");
                return;
            }
            let src =
                CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap, 0);
            if src.is_null() {
                crate::diag::log("delta tap: run-loop source creation failed");
                return;
            }
            let rl = CFRunLoopGetCurrent();
            CFRunLoopAddSource(rl, src, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            crate::diag::log("delta tap: armed (mouse motion deltas flowing)");
            CFRunLoopRun();
        });
        true
    }
}
