//! Control plane: which machine currently "has" the mouse, and the edge-crossing state machine.
//!
//! ## The model (a clean break from the old `rdev::listen` build)
//!
//! The capture layer *grabs* input: on macOS a `CGEventTap` returns `Drop`, so the OS never sees
//! the event and never clamps the cursor at a display edge. Because motion keeps arriving, we can
//! forward **relative deltas** and predict edge crossings from `location + delta` *before* the
//! cursor reaches the edge. There is no treadmill, no edge-rest poller, no park-and-bounce — those
//! were all band-aids for the observer architecture's inability to keep the delta stream alive when
//! the OS pinned the cursor.
//!
//! Two states:
//!
//! * **Local** — the real cursor is free. We only watch for a predicted crossing into a neighbour.
//! * **Forwarding(name)** — a secondary has control. Every event is dropped (macOS) and forwarded as
//!   a relative delta; the (hidden, parked) local cursor is kept against the shared edge so arrival
//!   position stays stable. When the delta reverses across the edge we return to Local.

use crate::layout::{Layout, Screen, Side};
use crate::network::Net;
use crate::protocol::{InputEvent, Message, MsButton};
use rdev::Key;
use std::sync::{Arc, Mutex};

/// A platform-neutral input event handed to the control plane by the capture layer.
///
/// Mouse motion carries the **relative delta** as reported by the OS (`kCGMouseEventDeltaX/Y` on
/// macOS, or `current − previous` derived from the rdev stream elsewhere).
#[derive(Debug, Clone)]
pub enum RawInput {
    Motion { dx: f64, dy: f64 },
    ButtonDown(MsButton),
    ButtonUp(MsButton),
    Wheel { dx: i64, dy: i64 },
    KeyDown(Key),
    KeyUp(Key),
}

/// Capture state of the *primary*: local control, or forwarding to a named secondary.
#[derive(Debug, Clone)]
pub enum CaptureMode {
    Local,
    Forwarding(String),
}

/// A secondary currently being driven (primary side), or the machine currently driving *us*
/// (secondary side). `vx`/`vy` is the virtual cursor position in the **receiver's own** coordinates.
#[derive(Debug, Clone)]
pub struct RemoteCtrl {
    pub name: String,
    pub side: Side,
    pub vx: f64,
    pub vy: f64,
}

/// Mutable control-plane state shared between the capture thread and the GUI.
#[derive(Debug, Default)]
pub struct Ctrl {
    pub init: bool,
    pub last_real: (f64, f64),
    /// `Some` while a secondary has control (primary) or while we are being driven (secondary).
    pub remote: Option<RemoteCtrl>,
    /// macOS primary: the local edge point the (hidden) cursor is parked at while forwarding.
    pub parked: Option<(f64, f64)>,
    /// Suppress automatic re-crossing until this instant (set right after control returns).
    pub cooldown_until: Option<std::time::Instant>,
    pub drops: u32,
    pub hk: HotkeyState,
    /// This host's own display rectangle(s), used to seed/clamp the virtual cursor on a secondary.
    pub local_bbox: Option<(f64, f64, f64, f64)>,
    /// Keys/buttons held while forwarding, so we can release them on the remote when control returns.
    pub held_keys: Vec<Key>,
    pub held_buttons: Vec<MsButton>,
}

#[derive(Debug, Default)]
pub struct HotkeyState {
    pub ctrl: bool,
    pub alt: bool,
}

/// Everything the capture layer + control plane need, shared across threads.
pub struct GrabCtx {
    pub net: Arc<Mutex<Net>>,
    pub layout: Arc<Mutex<Layout>>,
    pub ctrl: Arc<Mutex<Ctrl>>,
    pub mode: Mutex<CaptureMode>,
    pub my_name: String,
    pub primary_name: String,
}

/// A remote counts as attached just beyond an edge when its gap is within this distance. Generous
/// on purpose: tiles dragged in the canvas only line up roughly, and crossing only needs to know
/// which neighbour lies beyond the edge.
const EDGE_ATTACH: f64 = 240.0;
/// After control returns to the primary, automatic re-crossing is suppressed for this long — the
/// cursor parks just inside the shared edge, so a stray glide would otherwise re-cross instantly.
const RETURN_COOLDOWN_MS: u64 = 700;
/// Tolerance (px) for "is the cursor still beyond the shared edge" while forwarding.
const CROSS_EPS: f64 = 1.0;

/// Switch hotkey: **ScrollLock** (kept for compatibility) or **Ctrl+Alt+Space**.
///
/// Ctrl+Alt+Space is primary because most Mac keyboards have no ScrollLock key. Called for both
/// presses and releases so modifier state stays in sync; returns `true` only on the press that fires.
pub fn hotkey_fired(k: Key, down: bool, st: &mut HotkeyState) -> bool {
    match k {
        Key::ControlLeft | Key::ControlRight => st.ctrl = down,
        Key::Alt | Key::AltGr => st.alt = down,
        Key::ScrollLock => return down,
        Key::Space => return down && st.ctrl && st.alt,
        _ => {}
    }
    false
}

/// Entry point called by the capture layer for every event. `location` is the cursor's current
/// position (macOS: `event.location()`; elsewhere: the rdev `x,y`). Returns `true` if the capture
/// layer should **drop** the event (macOS grab only — other platforms ignore the return value).
pub fn on_capture(ctx: &GrabCtx, raw: RawInput, location: Option<(f64, f64)>) -> bool {
    let mode = ctx.mode.lock().unwrap().clone();
    match raw {
        RawInput::Motion { dx, dy } => {
            let mut c = ctx.ctrl.lock().unwrap();
            let l = ctx.layout.lock().unwrap();
            match mode {
                CaptureMode::Local => {
                    if let Some(t) = c.cooldown_until {
                        if std::time::Instant::now() < t {
                            return false;
                        }
                    }
                    let Some(bbox) = l.local_bbox() else { return false };
                    let Some(loc) = location else { return false };
                    if l.screens.len() <= 1 {
                        return false;
                    }
                    match predict_cross(&l, loc, (dx, dy), bbox) {
                        Some((side, name)) => {
                            enter_forwarding(ctx, &mut c, &l, side, &name, loc);
                            forward_motion(ctx, &name, dx, dy);
                            true
                        }
                        None => false,
                    }
                }
                CaptureMode::Forwarding(name) => {
                    let bbox = l.local_bbox();
                    let loc = location;
                    match (bbox, loc) {
                        (Some(bbox), Some(loc)) if still_outside(&c, loc, (dx, dy), bbox) => {
                            forward_motion(ctx, &name, dx, dy);
                            if let Some(park) = c.parked {
                                crate::capture::park_cursor(park);
                            }
                            true
                        }
                        _ => {
                            // Return to local (cursor came back across the edge, or we lost
                            // position data). Release any held keys/buttons on the remote first.
                            leave_forwarding(ctx, &mut c, &l, &name, loc);
                            false
                        }
                    }
                }
            }
        }
        RawInput::ButtonDown(b) => {
            let dropped = forward_if_forwarding(ctx, &mode, InputEvent::MouseDown { button: b.clone() });
            if dropped {
                ctx.ctrl.lock().unwrap().held_buttons.push(b);
            }
            dropped
        }
        RawInput::ButtonUp(b) => {
            let dropped = forward_if_forwarding(ctx, &mode, InputEvent::MouseUp { button: b.clone() });
            if dropped {
                let mut c = ctx.ctrl.lock().unwrap();
                c.held_buttons.retain(|x| *x != b);
            }
            dropped
        }
        RawInput::Wheel { dx, dy } => forward_if_forwarding(ctx, &mode, InputEvent::Wheel { dx, dy }),
        RawInput::KeyDown(k) => {
            if let CaptureMode::Local = mode {
                let fired = {
                    let mut c = ctx.ctrl.lock().unwrap();
                    hotkey_fired(k, true, &mut c.hk)
                };
                if fired {
                    cycle_control(ctx);
                    return false;
                }
            }
            let dropped = forward_if_forwarding(ctx, &mode, InputEvent::KeyDown { key: k.clone() });
            if dropped {
                ctx.ctrl.lock().unwrap().held_keys.push(k);
            }
            dropped
        }
        RawInput::KeyUp(k) => {
            if let CaptureMode::Local = mode {
                let mut c = ctx.ctrl.lock().unwrap();
                hotkey_fired(k, false, &mut c.hk);
                if k == Key::ScrollLock {
                    return false;
                }
            }
            let dropped = forward_if_forwarding(ctx, &mode, InputEvent::KeyUp { key: k.clone() });
            if dropped {
                let mut c = ctx.ctrl.lock().unwrap();
                c.held_keys.retain(|x| *x != k);
            }
            dropped
        }
    }
}

/// Forward a single input event to the secondary we're controlling. Returns `true` if we are
/// forwarding (so the caller should drop the local event), `false` otherwise.
fn forward_if_forwarding(ctx: &GrabCtx, mode: &CaptureMode, ev: InputEvent) -> bool {
    if let CaptureMode::Forwarding(name) = mode {
        ctx.net.lock().unwrap().send_input(&name, ev);
        true
    } else {
        false
    }
}

fn forward_motion(ctx: &GrabCtx, name: &str, dx: f64, dy: f64) {
    ctx.net.lock().unwrap().send_input(name, InputEvent::MouseMotion { dx, dy });
}

/// Predict a crossing: is `location + delta` beyond a local-bbox edge that has a neighbour attached
/// just beyond it? Returns that side and the neighbour's name.
fn predict_cross(
    l: &Layout,
    loc: (f64, f64),
    delta: (f64, f64),
    bbox: (f64, f64, f64, f64),
) -> Option<(Side, String)> {
    let (bl, bt, br, bb) = bbox;
    let px = loc.0 + delta.0;
    let py = loc.1 + delta.1;
    for s in l.screens.iter().filter(|s| !s.is_local) {
        let sl = s.ox as f64;
        let st = s.oy as f64;
        let sr = sl + s.w as f64;
        let sb = st + s.h as f64;
        let overlap_v = st < bb && sb > bt;
        let overlap_h = sl < br && sr > bl;
        if px >= br && sl >= br - EDGE_ATTACH && overlap_v {
            return Some((Side::Right, s.name.clone()));
        }
        if px <= bl && sr <= bl + EDGE_ATTACH && overlap_v {
            return Some((Side::Left, s.name.clone()));
        }
        if py >= bb && st >= bb - EDGE_ATTACH && overlap_h {
            return Some((Side::Bottom, s.name.clone()));
        }
        if py <= bt && sb <= bt + EDGE_ATTACH && overlap_h {
            return Some((Side::Top, s.name.clone()));
        }
    }
    None
}

/// While forwarding on `side`, is the predicted position still beyond the shared edge (i.e. still
/// outside, so we should keep forwarding)? If it has reversed back inside, we return to local.
fn still_outside(c: &Ctrl, loc: (f64, f64), delta: (f64, f64), bbox: (f64, f64, f64, f64)) -> bool {
    let side = match &c.remote {
        Some(r) => r.side,
        None => return false,
    };
    let (bl, bt, br, bb) = bbox;
    let px = loc.0 + delta.0;
    let py = loc.1 + delta.1;
    match side {
        Side::Right => px >= br - CROSS_EPS,
        Side::Left => px <= bl + CROSS_EPS,
        Side::Bottom => py >= bb - CROSS_EPS,
        Side::Top => py <= bt + CROSS_EPS,
    }
}

/// Begin forwarding control to `name` (attached on `side`). Hides the local cursor, parks it at the
/// shared edge, and tells the secondary the cursor is entering (so it can seed its own virtual cursor).
fn enter_forwarding(
    ctx: &GrabCtx,
    c: &mut Ctrl,
    l: &Layout,
    side: Side,
    name: &str,
    loc: (f64, f64),
) {
    let (bl, bt, br, bb) = match l.local_bbox() {
        Some(b) => b,
        None => return,
    };
    let fx = ((loc.0 - bl) / (br - bl)).clamp(0.0, 1.0);
    let fy = ((loc.1 - bt) / (bb - bt)).clamp(0.0, 1.0);
    let park = match side {
        Side::Right => (br - 1.0, bt + fy * (bb - bt)),
        Side::Left => (bl + 1.0, bt + fy * (bb - bt)),
        Side::Bottom => (bl + fx * (br - bl), bb - 1.0),
        Side::Top => (bl + fx * (br - bl), bt + 1.0),
    };
    c.remote = Some(RemoteCtrl {
        name: name.to_string(),
        side,
        vx: park.0,
        vy: park.1,
    });
    c.parked = Some(park);
    c.held_keys.clear();
    c.held_buttons.clear();
    c.cooldown_until = None;
    *ctx.mode.lock().unwrap() = CaptureMode::Forwarding(name.to_string());
    crate::capture::hide_cursor();
    ctx.net.lock().unwrap().send_to(
        name,
        Message::EnterScreen {
            side,
            fx,
            fy,
        },
    );
    log::info!("control handed to {} ({:?})", name, side);
    crate::diag::log(&format!(
        "HAND-OFF -> {} side={:?} park=({:.0},{:.0})",
        name, side, park.0, park.1
    ));
}

/// Return control to the local machine: release any held keys/buttons on the remote, show the local
/// cursor, park it just inside the shared edge, and tell the secondary control left.
fn leave_forwarding(
    ctx: &GrabCtx,
    c: &mut Ctrl,
    l: &Layout,
    name: &str,
    loc: Option<(f64, f64)>,
) {
    // Release held keys/buttons on the remote so it never keeps a "ghost" modifier down.
    {
        let net = ctx.net.lock().unwrap();
        for k in c.held_keys.drain(..) {
            net.send_input(name, InputEvent::KeyUp { key: k });
        }
        for b in c.held_buttons.drain(..) {
            net.send_input(name, InputEvent::MouseUp { button: b });
        }
    }
    let side = c.remote.as_ref().map(|r| r.side);
    c.remote = None;
    c.parked = None;
    *ctx.mode.lock().unwrap() = CaptureMode::Local;
    c.cooldown_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(RETURN_COOLDOWN_MS));
    crate::capture::show_cursor();
    if let (Some(side), Some((bl, bt, br, bb))) = (side, l.local_bbox()) {
        let (fx, fy) = match loc {
            Some(p) => (
                ((p.0 - bl) / (br - bl)).clamp(0.0, 1.0),
                ((p.1 - bt) / (bb - bt)).clamp(0.0, 1.0),
            ),
            None => (0.5, 0.5),
        };
        let target = match side {
            Side::Right => (br - 12.0, bt + fy * (bb - bt)),
            Side::Left => (bl + 12.0, bt + fy * (bb - bt)),
            Side::Bottom => (bl + fx * (br - bl), bb - 12.0),
            Side::Top => (bl + fx * (br - bl), bt + 12.0),
        };
        crate::input::warp_cursor(target.0, target.1);
        c.last_real = target;
    }
    ctx.net.lock().unwrap().send_to(name, Message::LeaveScreen);
    log::info!("control returned to {}", ctx.primary_name);
    crate::diag::log(&format!("RETURN <- {}", name));
}

/// Which outer edge of the local bbox is `s` attached just beyond?
fn attached_side(s: &Screen, bbox: (f64, f64, f64, f64)) -> Option<Side> {
    let (bl, bt, br, bb) = bbox;
    let sl = s.ox as f64;
    let st = s.oy as f64;
    let sr = sl + s.w as f64;
    let sb = st + s.h as f64;
    if sl >= br - EDGE_ATTACH {
        Some(Side::Right)
    } else if sr <= bl + EDGE_ATTACH {
        Some(Side::Left)
    } else if st >= bb - EDGE_ATTACH {
        Some(Side::Bottom)
    } else if sb <= bt + EDGE_ATTACH {
        Some(Side::Top)
    } else {
        None
    }
}

/// Rotate control: local → each secondary → back to local. Invoked by the hotkey on the primary.
pub fn cycle_control(ctx: &GrabCtx) {
    let mut c = ctx.ctrl.lock().unwrap();
    let l = ctx.layout.lock().unwrap();
    if l.screens.len() <= 1 {
        return;
    }
    let Some(bbox) = l.local_bbox() else { return };
    let mut remotes: Vec<&Screen> = Vec::new();
    for s in l.screens.iter() {
        if !s.is_local && !remotes.iter().any(|r| r.name == s.name) {
            remotes.push(s);
        }
    }
    if remotes.is_empty() {
        return;
    }
    let idx = match &c.remote {
        Some(r) => remotes
            .iter()
            .position(|s| s.name == r.name)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };
    if idx >= remotes.len() {
        // Wrap around: return to local.
        if let Some(r) = c.remote.clone() {
            let loc = c.parked;
            leave_forwarding(ctx, &mut c, &l, &r.name, loc);
        }
        return;
    }
    // Leave the current secondary first (release held keys), then enter the next.
    if let Some(r) = c.remote.clone() {
        let loc = c.parked;
        leave_forwarding(ctx, &mut c, &l, &r.name, loc);
    }
    let s = remotes[idx];
    let side = attached_side(s, bbox).unwrap_or(Side::Right);
    let loc = crate::input::cursor_position().unwrap_or((0.0, 0.0));
    enter_forwarding(ctx, &mut c, &l, side, &s.name, loc);
}

// ---- Secondary side: receiving control ----

/// The primary says the cursor is entering our screen. Seed our virtual cursor at the edge facing
/// the primary and hide our real cursor.
pub fn on_enter_screen(ctx: &GrabCtx, side: Side, fx: f64, fy: f64) {
    let mut c = ctx.ctrl.lock().unwrap();
    let Some((bl, bt, br, bb)) = c.local_bbox else { return };
    let eside = side.opposite();
    let (vx, vy) = match eside {
        Side::Right => (br - 2.0, bt + fy * (bb - bt)),
        Side::Left => (bl + 2.0, bt + fy * (bb - bt)),
        Side::Bottom => (bl + fx * (br - bl), bb - 2.0),
        Side::Top => (bl + fx * (br - bl), bt + 2.0),
    };
    c.remote = Some(RemoteCtrl {
        name: ctx.primary_name.clone(),
        side: eside,
        vx,
        vy,
    });
    drop(c);
    crate::capture::hide_cursor();
    crate::diag::log(&format!(
        "ENTER <- primary side={:?} seed=({:.0},{:.0})",
        eside, vx, vy
    ));
}

/// The primary says control left our screen. Show our cursor again.
pub fn on_leave_screen(ctx: &GrabCtx) {
    let mut c = ctx.ctrl.lock().unwrap();
    c.remote = None;
    drop(c);
    crate::capture::show_cursor();
    crate::diag::log("LEAVE -> primary");
}

/// Apply a forwarded input event while we are being driven. Motion is relative: we accumulate the
/// delta against our own virtual cursor and warp to the result (clamped to our own displays).
pub fn on_secondary_input(ctx: &GrabCtx, ev: InputEvent) {
    let mut c = ctx.ctrl.lock().unwrap();
    let bbox = c.local_bbox;
    let Some(r) = c.remote.as_mut() else {
        return; // not being driven — ignore stray events
    };
    match ev {
        InputEvent::MouseMotion { dx, dy } => {
            r.vx += dx;
            r.vy += dy;
            if let Some((bl, bt, br, bb)) = bbox {
                r.vx = r.vx.clamp(bl, br - 1.0);
                r.vy = r.vy.clamp(bt, bb - 1.0);
            }
            crate::input::warp_cursor(r.vx, r.vy);
        }
        other => crate::input::apply_input(&other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Screen {
        Screen {
            name: "A".into(),
            ox: 0,
            oy: 0,
            w: 1920,
            h: 1080,
            is_local: true,
            scale: 1.0,
        }
    }

    fn remote(name: &str, ox: i32, oy: i32, w: u32, h: u32) -> Screen {
        Screen {
            name: name.into(),
            ox,
            oy,
            w,
            h,
            is_local: false,
            scale: 1.0,
        }
    }

    const BBOX: (f64, f64, f64, f64) = (0.0, 0.0, 1920.0, 1080.0);

    #[test]
    fn predict_cross_right() {
        let l = Layout {
            screens: vec![local(), remote("B", 1920, 0, 1920, 1080)],
        };
        // Cursor at the right edge moving right -> cross into B.
        assert_eq!(
            predict_cross(&l, (1918.0, 540.0), (10.0, 0.0), BBOX),
            Some((Side::Right, "B".to_string()))
        );
        // Not reaching the edge -> no cross.
        assert_eq!(predict_cross(&l, (1000.0, 540.0), (10.0, 0.0), BBOX), None);
        // Moving back inside -> no cross.
        assert_eq!(predict_cross(&l, (1918.0, 540.0), (-10.0, 0.0), BBOX), None);
    }

    #[test]
    fn predict_cross_left() {
        let l = Layout {
            screens: vec![local(), remote("L", -1920, 0, 1920, 1080)],
        };
        assert_eq!(
            predict_cross(&l, (5.0, 540.0), (-10.0, 0.0), BBOX),
            Some((Side::Left, "L".to_string()))
        );
    }

    #[test]
    fn predict_cross_bottom() {
        let l = Layout {
            screens: vec![local(), remote("D", 0, 1080, 1920, 1080)],
        };
        assert_eq!(
            predict_cross(&l, (960.0, 1075.0), (0.0, 10.0), BBOX),
            Some((Side::Bottom, "D".to_string()))
        );
    }

    #[test]
    fn predict_cross_top() {
        let l = Layout {
            screens: vec![local(), remote("U", 0, -1080, 1920, 1080)],
        };
        assert_eq!(
            predict_cross(&l, (960.0, 5.0), (0.0, -10.0), BBOX),
            Some((Side::Top, "U".to_string()))
        );
    }

    #[test]
    fn predict_cross_only_remote_neighbours() {
        // No remote screen attached -> never cross, even at the edge.
        let l = Layout { screens: vec![local()] };
        assert_eq!(predict_cross(&l, (1918.0, 540.0), (10.0, 0.0), BBOX), None);
    }

    #[test]
    fn predict_cross_no_vertical_overlap() {
        // A right-hand remote that does NOT vertically overlap the local bbox must not cross.
        let l = Layout {
            screens: vec![local(), remote("B", 1920, 2000, 1920, 1080)],
        };
        assert_eq!(predict_cross(&l, (1918.0, 540.0), (10.0, 0.0), BBOX), None);
    }

    #[test]
    fn hotkey_scrolllock_fires_on_press_only() {
        let mut st = HotkeyState::default();
        assert!(hotkey_fired(Key::ScrollLock, true, &mut st));
        assert!(!hotkey_fired(Key::ScrollLock, false, &mut st));
    }

    #[test]
    fn hotkey_ctrl_alt_space_fires() {
        let mut st = HotkeyState::default();
        assert!(!hotkey_fired(Key::ControlLeft, true, &mut st));
        assert!(!hotkey_fired(Key::Alt, true, &mut st));
        assert!(hotkey_fired(Key::Space, true, &mut st));
        // Releasing space must not re-fire.
        assert!(!hotkey_fired(Key::Space, false, &mut st));
    }
}
