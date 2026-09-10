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
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// A platform-neutral input event handed to the control plane by the capture layer.
///
/// Mouse motion carries the **relative delta** as reported by the OS (`kCGMouseEventDeltaX/Y` on
/// macOS, or `current − previous` derived from the rdev stream elsewhere).
#[derive(Debug, Clone)]
pub enum RawInput {
    Motion {
        dx: f64,
        dy: f64,
    },
    ButtonDown(MsButton),
    ButtonUp(MsButton),
    Wheel {
        dx: i64,
        dy: i64,
    },
    KeyDown(Key),
    KeyUp(Key),
    /// Current modifier state (macOS reports these via FlagsChanged, never KeyDown/KeyUp).
    /// Feeds the switch-hotkey's Ctrl+Alt tracker; never itself consumed or dropped.
    Mods {
        ctrl: bool,
        alt: bool,
    },
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
    /// The *machine* being driven (or driving us) — the name input is routed to. One machine may
    /// own several panels; the unit of routing is always the machine.
    pub name: String,
    /// The specific panel of `name` the cursor entered, when the hub knows it. On the secondary
    /// side this is what decides *where* the cursor is seeded and, more importantly, *which* edge
    /// hands control back: with several local monitors only the panel the hub actually attached
    /// the cursor to can return it. `None` for peers that predate the field, or for a hand-off
    /// started by the hotkey rather than by crossing — the code then falls back to the group
    /// bounding box.
    pub panel: Option<String>,
    pub side: Side,
    pub vx: f64,
    pub vy: f64,
    /// Multiplier applied to every forwarded mouse delta: `own_scale / remote_scale`.
    ///
    /// Mouse deltas arrive in the *source* machine's coordinate units — points on a Retina Mac
    /// (1 point = 2 physical pixels) but physical pixels on a DPI-aware Windows box. Shipping
    /// them verbatim makes the cursor crawl on the higher-resolution side: a Retina primary
    /// crossing to a 1x secondary moved at half speed. Converting to the receiver's units (i.e.
    /// to physical pixels on both sides) keeps the cursor speed identical across machines.
    ///
    /// Captured once when control is handed over so the speed never changes mid-forwarding.
    pub scale_ratio: f64,
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
    /// The rect of the local panel that was being left when the hand-off happened. Returning the
    /// cursor relative to *it* keeps it on the display the user left, on a multi-monitor machine.
    pub parked_rect: Option<(f64, f64, f64, f64)>,
    /// Suppress automatic re-crossing until this instant (set right after control returns).
    pub cooldown_until: Option<std::time::Instant>,
    pub drops: u32,
    pub hk: HotkeyState,
    /// This host's own display rectangle(s), used to seed/clamp the virtual cursor on a secondary.
    pub local_bbox: Option<(f64, f64, f64, f64)>,
    /// Keys/buttons held while forwarding, so we can release them on the remote when control returns.
    pub held_keys: Vec<Key>,
    pub held_buttons: Vec<MsButton>,
    /// A snapshot of the layout, refreshed by a background thread. The capture callback runs
    /// inside the macOS event tap, where blocking on the GUI's layout mutex can stall long
    /// enough for the OS to disable the tap (`TapDisabledByTimeout`) — and while the tap is
    /// disabled events are no longer dropped, so **both** machines' cursors move at once.
    /// Reading an `Arc` snapshot keeps the hot path lock-free.
    pub layout_snap: Arc<Layout>,
    /// This machine's own displays. A secondary may have more than one, and the virtual cursor
    /// has to roam between them (and only hand control back at the outermost one).
    pub local_screens: Vec<Screen>,
    /// Names of the currently reachable peers, refreshed alongside [`Ctrl::layout_snap`].
    ///
    /// The crossing decision needs to know whether the machine on the far side of an edge is
    /// actually connected. The layout is *persisted*, so a saved tile can easily outlive the peer it
    /// describes — and handing off to a machine that is not there would hide the local cursor and
    /// forward into nothing until the next event noticed. Reading this from the same lock-free
    /// snapshot the tap already clones keeps that check off the mutex.
    pub live: Arc<Vec<String>>,
}

#[derive(Debug, Default)]
pub struct HotkeyState {
    pub ctrl: bool,
    pub alt: bool,
}

/// An input event the capture callback wants sent to a peer.
pub struct OutboundInput {
    pub target: String,
    pub ev: InputEvent,
}

/// Everything the capture layer + control plane need, shared across threads.
pub struct GrabCtx {
    pub net: Arc<Mutex<Net>>,
    pub layout: Arc<Mutex<Layout>>,
    pub ctrl: Arc<Mutex<Ctrl>>,
    pub mode: Mutex<CaptureMode>,
    pub my_name: String,
    pub primary_name: String,
    /// Lock-free send path for forwarded input.
    ///
    /// **Why this exists.** Forwarded mouse motion is produced inside the macOS event-tap
    /// callback, which runs in the OS input pipeline. Sending it meant locking `net`, and any
    /// wait on that lock stalls the tap; macOS then declares it unresponsive and disables it
    /// (`TapDisabledByTimeout`). A disabled tap stops *dropping* events while we are still in
    /// `Forwarding`, so the local cursor starts moving again while the remote one keeps being
    /// driven — the "both cursors move at once" bug.
    ///
    /// With this channel the callback only does an unbounded, allocation-light `send` (no locks
    /// that anything else holds for long), and a pump thread does the real `net`-locked send.
    /// `None` in tests, which have no event tap to stall and so send inline.
    pub input_tx: Option<Sender<OutboundInput>>,
    /// Screen-space rect of MouseShare's own main window, published by the GUI every frame.
    ///
    /// While a secondary controls this machine, every local click is dropped and forwarded —
    /// which used to make MouseShare's own window (and everything around it) unclickable until
    /// control came back, the "the app's UI is dead" complaint. A click that lands inside this
    /// rect instead returns control and is delivered locally (ShareMouse behaves the same).
    /// Read with `try_lock` from the tap callback; the GUI only ever holds it for an assign.
    pub ui_window_rect: Mutex<Option<(f64, f64, f64, f64)>>,
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
/// How far outside a display's own rectangle the cursor may sit and still count as "on" it.
const ON_SCREEN_TOL: f64 = 2.0;
/// Safety bounds for the forwarded-delta scale ratio (see `motion_scale_ratio`).
const MIN_SCALE_RATIO: f64 = 0.25;
const MAX_SCALE_RATIO: f64 = 4.0;
/// Bounds for the combined ratio once the user's manual multiplier is applied.
const MIN_EFFECTIVE_RATIO: f64 = 0.1;
const MAX_EFFECTIVE_RATIO: f64 = 8.0;

/// User override multiplied on top of the auto-derived ratio (`Config::motion_scale`, default
/// 1.0). Stored as raw `f32` bits so it can be updated from the GUI without a lock.
static MOTION_SCALE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1.0f32.to_bits());

/// Set the user's manual speed multiplier (1.0 = leave the automatic ratio untouched).
pub fn set_motion_scale(v: f32) {
    let v = if v.is_finite() {
        v.clamp(0.1, 8.0)
    } else {
        1.0
    };
    MOTION_SCALE.store(v.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// The user's manual speed multiplier.
pub fn motion_scale() -> f32 {
    f32::from_bits(MOTION_SCALE.load(std::sync::atomic::Ordering::Relaxed))
}

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
            // Snapshot instead of `ctx.layout.lock()`: the event-tap callback must never block
            // on a lock the GUI thread might be holding (see `Ctrl::layout_snap`).
            let snap = c.layout_snap.clone();
            let l: &Layout = &snap;
            match mode {
                CaptureMode::Local => {
                    if let Some(t) = c.cooldown_until {
                        if std::time::Instant::now() < t {
                            return false;
                        }
                    }
                    let Some(loc) = location else { return false };
                    if !l.screens.iter().any(|s| !s.is_local) {
                        return false;
                    }
                    match predict_cross(l, loc, (dx, dy)) {
                        // Never hand off to a machine we are not connected to: see `Ctrl::live`.
                        // The window before this check was added was the one way to strand the
                        // pointer with no way back short of moving the mouse again.
                        Some(cross) if c.live.iter().any(|n| n == &cross.host) => {
                            let host = cross.host.clone();
                            enter_forwarding(ctx, &mut c, l, &cross, loc);
                            // Read the ratio *after* the hand-off: that is where it is computed.
                            let ratio = c.remote.as_ref().map_or(1.0, |r| r.scale_ratio);
                            forward_motion(ctx, &host, dx * ratio, dy * ratio);
                            true
                        }
                        _ => false,
                    }
                }
                CaptureMode::Forwarding(name) => {
                    // While a secondary has control every delta is forwarded and the local event
                    // dropped. The RETURN decision lives on the secondary: it tracks its own
                    // virtual cursor (clamped to its own screen) and sends `ReturnControl` when
                    // that cursor is pushed back across the shared edge. The primary's real cursor
                    // is frozen at the edge here (events are dropped), so its position is *not* a
                    // valid signal for deciding when to come back — that was the old bug where any
                    // leftward twitch on the secondary snapped control straight back to the Mac.
                    //
                    // The liveness probe uses `try_lock` and treats "busy" as alive: this runs on
                    // the event-tap thread at 100+ Hz, and `net` is momentarily held by the input
                    // pump for every frame. Blocking here would stall the tap, get it disabled by
                    // macOS, and — with Drop verdicts ignored — both cursors move at once.
                    let alive = ctx
                        .net
                        .try_lock()
                        .map(|n| n.has_peer(&name))
                        .unwrap_or(true);
                    if !alive {
                        // The secondary dropped mid-hand-off; return rather than forward into a
                        // dead socket (which would leave the cursor hidden and stuck).
                        leave_forwarding(ctx, &mut c, &l, &name, location);
                        false
                    } else {
                        let ratio = c.remote.as_ref().map_or(1.0, |r| r.scale_ratio);
                        forward_motion(ctx, &name, dx * ratio, dy * ratio);
                        if let Some(park) = c.parked {
                            crate::capture::park_cursor(park);
                        }
                        true
                    }
                }
            }
        }
        RawInput::ButtonDown(b) => {
            // While a secondary has control, a click that lands inside MouseShare's own window
            // returns control instead of being swallowed and forwarded — otherwise the app's
            // UI (language toggle, Quit, …) is unclickable for as long as forwarding lasts,
            // which read as "the button is broken". The remote gets a MouseUp for the button
            // we never let down remotely, so it is not left with a stuck press.
            if let CaptureMode::Forwarding(name) = &mode {
                let inside_ui = location
                    .zip(ctx.ui_window_rect.try_lock().ok().and_then(|g| *g))
                    .is_some_and(|((x, y), (l, t, r, btm))| x >= l && x <= r && y >= t && y <= btm);
                if inside_ui {
                    let mut c = ctx.ctrl.lock().unwrap();
                    let snap = c.layout_snap.clone();
                    // leave_forwarding releases any remotely-held buttons/keys, so the remote
                    // is never left with a stuck press.
                    leave_forwarding(ctx, &mut c, &snap, name, location);
                    crate::diag::log("CLICK INSIDE UI WINDOW while forwarding — control returned");
                    return false;
                }
            }
            let dropped =
                forward_if_forwarding(ctx, &mode, InputEvent::MouseDown { button: b.clone() });
            if dropped {
                ctx.ctrl.lock().unwrap().held_buttons.push(b);
            }
            dropped
        }
        RawInput::ButtonUp(b) => {
            let dropped =
                forward_if_forwarding(ctx, &mode, InputEvent::MouseUp { button: b.clone() });
            if dropped {
                let mut c = ctx.ctrl.lock().unwrap();
                c.held_buttons.retain(|x| *x != b);
            }
            dropped
        }
        RawInput::Wheel { dx, dy } => {
            forward_if_forwarding(ctx, &mode, InputEvent::Wheel { dx, dy })
        }
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
        RawInput::Mods { ctrl, alt } => {
            // macOS delivers modifier state via FlagsChanged only, so this is the only way the
            // Ctrl+Alt+Space hotkey's modifier tracker ever learns Ctrl/Alt went down.
            let mut c = ctx.ctrl.lock().unwrap();
            c.hk.ctrl = ctrl;
            c.hk.alt = alt;
            // Never consumed: modifiers must keep reaching the OS and any remote.
            false
        }
    }
}

/// Forward a single input event to the secondary we're controlling. Returns `true` if we are
/// forwarding (so the caller should drop the local event), `false` otherwise.
///
/// The send itself goes through `GrabCtx::input_tx` when there is one, so the event-tap callback
/// never has to take the `net` lock (see that field's docs).
fn forward_if_forwarding(ctx: &GrabCtx, mode: &CaptureMode, ev: InputEvent) -> bool {
    if let CaptureMode::Forwarding(name) = mode {
        forward_to(ctx, name, ev);
        true
    } else {
        false
    }
}

/// Queue (or, in tests, directly send) one input event to `name`.
fn forward_to(ctx: &GrabCtx, name: &str, ev: InputEvent) {
    match &ctx.input_tx {
        Some(tx) => {
            // Unbounded channel: this cannot block. A dropped receiver means we are shutting
            // down, and losing a forwarded event then is harmless.
            let _ = tx.send(OutboundInput {
                target: name.to_string(),
                ev,
            });
        }
        None => ctx.net.lock().unwrap().send_input(name, ev),
    }
}

fn forward_motion(ctx: &GrabCtx, name: &str, dx: f64, dy: f64) {
    forward_to(ctx, name, InputEvent::MouseMotion { dx, dy });
}

fn screen_rect(s: &Screen) -> (f64, f64, f64, f64) {
    (
        s.ox as f64,
        s.oy as f64,
        s.ox as f64 + s.w as f64,
        s.oy as f64 + s.h as f64,
    )
}

/// The local display the point sits on (within a pixel or two), falling back to the nearest one.
pub fn local_screen_at<'a>(l: &'a Layout, x: f64, y: f64) -> Option<&'a Screen> {
    l.screens
        .iter()
        .filter(|s| s.is_local)
        .find(|s| {
            let (a, b, c, d) = screen_rect(s);
            x >= a - ON_SCREEN_TOL
                && x <= c + ON_SCREEN_TOL
                && y >= b - ON_SCREEN_TOL
                && y <= d + ON_SCREEN_TOL
        })
        .or_else(|| {
            let mut best: Option<&Screen> = None;
            let mut best_d = f64::MAX;
            for s in l.screens.iter().filter(|s| s.is_local) {
                let (a, b, c, d) = screen_rect(s);
                // Distance to the rectangle (0 when inside).
                let dx = (a - x).max(0.0).max(x - c);
                let dy = (b - y).max(0.0).max(y - d);
                let dist = dx * dx + dy * dy;
                if dist < best_d {
                    best_d = dist;
                    best = Some(s);
                }
            }
            best
        })
}

/// Where a predicted crossing lands: which machine to forward to, which of its panels the cursor
/// appears on, and which side of the *local* panel it left from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crossing {
    pub side: Side,
    /// The machine to route input to (`Screen::host`).
    pub host: String,
    /// The remote panel the cursor lands on (told to the receiver so a multi-monitor client seeds
    /// on the right display and knows which edge returns control).
    pub panel: String,
}

/// Predict a crossing: is `location + delta` leaving **the display the cursor is actually on**,
/// toward a neighbour that sits immediately beyond that display's edge?
///
/// This is deliberately per-display rather than per-machine. Using the union bounding box of a
/// multi-monitor Mac lets the cursor "leave" from a dead corner of the bbox, or jump straight
/// from the far display to a remote machine that is only attached to the near one — both show
/// up as the hand-off firing while the cursor is still in the middle of the desktop, which is
/// exactly what makes two cursors move at once.
///
/// The *result* is per-machine (`host`) plus the exact remote panel, so a machine with several
/// monitors routes to one socket while still telling that machine which panel it landed on.
fn predict_cross(l: &Layout, loc: (f64, f64), delta: (f64, f64)) -> Option<Crossing> {
    // The cursor must be on one of *our* displays for it to leave that display.
    let ls = local_screen_at(l, loc.0, loc.1)?;
    let (ll, lt, lr, lb) = screen_rect(ls);
    if loc.0 < ll - ON_SCREEN_TOL
        || loc.0 > lr + ON_SCREEN_TOL
        || loc.1 < lt - ON_SCREEN_TOL
        || loc.1 > lb + ON_SCREEN_TOL
    {
        return None;
    }
    let px = loc.0 + delta.0;
    let py = loc.1 + delta.1;
    for rs in l.screens.iter().filter(|s| !s.is_local) {
        let (sl, st, sr, sb) = screen_rect(rs);
        // The neighbour must be *flush* against this display's edge (within EDGE_ATTACH) and
        // overlap it along the crossing axis — otherwise it is not the screen we'd land on.
        let overlaps_v = sb > lt && st < lb;
        let overlaps_h = sr > ll && sl < lr;
        let hit = |side: Side| {
            Some(Crossing {
                side,
                host: rs.host().to_string(),
                panel: rs.name.clone(),
            })
        };
        if px >= lr && sl >= lr - EDGE_ATTACH && sl < lr + EDGE_ATTACH && overlaps_v {
            return hit(Side::Right);
        }
        if px <= ll && sr <= ll + EDGE_ATTACH && sr > ll - EDGE_ATTACH && overlaps_v {
            return hit(Side::Left);
        }
        if py >= lb && st >= lb - EDGE_ATTACH && st < lb + EDGE_ATTACH && overlaps_h {
            return hit(Side::Bottom);
        }
        if py <= lt && sb <= lt + EDGE_ATTACH && sb > lt - EDGE_ATTACH && overlaps_h {
            return hit(Side::Top);
        }
    }
    None
}

/// The secondary's virtual cursor should hand control back when it is being pushed back across the
/// edge that faces the primary. `r.side` is that edge (the primary crossed its own side, so the
/// secondary is entered on the opposite edge). Returns true when the cursor, after applying
/// `(dx, dy)`, would pass that edge while still moving toward the primary.
///
/// The test is **per panel**, not against the machine's bounding box. On a client with two
/// monitors the bbox spans dead space: pressing "toward the primary" while on the monitor that
/// does *not* touch the primary would otherwise be misread as a return, which snaps control away
/// mid-movement. Only `r.panel` — the display the hub actually attached the cursor to — has an
/// edge that faces the primary, so only it can hand control back.
fn crossing_back(r: &RemoteCtrl, screens: &[Screen], dx: f64, dy: f64) -> bool {
    let panel = r
        .panel
        .as_ref()
        .and_then(|name| screens.iter().find(|s| &s.name == name))
        .or_else(|| {
            // No panel hint (older peer, or a hotkey hand-off): use the panel under the cursor.
            screens
                .iter()
                .find(|s| s.contains(r.vx, r.vy))
                .or_else(|| nearest_screen(screens, r.vx, r.vy))
        });
    match panel {
        Some(p) => {
            let (pl, pt, pr, pb) = screen_rect(p);
            match r.side {
                Side::Left => r.vx + dx <= pl + CROSS_EPS && dx < 0.0,
                Side::Right => r.vx + dx >= pr - 1.0 - CROSS_EPS && dx > 0.0,
                Side::Top => r.vy + dy <= pt + CROSS_EPS && dy < 0.0,
                Side::Bottom => r.vy + dy >= pb - 1.0 - CROSS_EPS && dy > 0.0,
            }
        }
        // We know of no panel at all (layout not yet pushed): fall back to the machine bbox, which
        // is what a single-monitor client has anyway.
        None => match r.side {
            Side::Left => r.vx + dx <= CROSS_EPS && dx < 0.0,
            Side::Right => r.vx + dx >= f64::MAX - CROSS_EPS && dx > 0.0,
            Side::Top => r.vy + dy <= CROSS_EPS && dy < 0.0,
            Side::Bottom => r.vy + dy >= f64::MAX - CROSS_EPS && dy > 0.0,
        },
    }
}

/// The panel of a machine that a cursor entering from `side` (a side of *our* bounding box) lands
/// on: always the machine's outermost panel on that axis, i.e. the one closest to us.
fn entry_panel(l: &Layout, host: &str, side: Side) -> Option<String> {
    let mut best: Option<(&Screen, i64)> = None;
    for s in l.panels_of(host) {
        let key = match side {
            Side::Right => s.ox as i64,
            Side::Left => -(s.ox as i64 + s.w as i64),
            Side::Bottom => s.oy as i64,
            Side::Top => -(s.oy as i64 + s.h as i64),
        };
        if best.map_or(true, |(_, bk)| key < bk) {
            best = Some((s, key));
        }
    }
    best.map(|(s, _)| s.name.clone())
}

/// Nearest panel to a point (used only as a last resort when the cursor sits in dead space).
fn nearest_screen(screens: &[Screen], x: f64, y: f64) -> Option<&Screen> {
    let mut best: Option<&Screen> = None;
    let mut best_d = f64::MAX;
    for s in screens {
        let (a, b, c, d) = screen_rect(s);
        let dx = (a - x).max(0.0).max(x - c);
        let dy = (b - y).max(0.0).max(y - d);
        let dist = dx * dx + dy * dy;
        if dist < best_d {
            best_d = dist;
            best = Some(s);
        }
    }
    best
}

/// Convert a local mouse delta into the receiving machine's coordinate units.
///
/// `kCGMouseEventDeltaX/Y` (and the rdev delta elsewhere) is measured in the *source* machine's
/// logical space: on a Retina Mac 1 point is 2 physical pixels, while a DPI-aware Windows box
/// counts physical pixels directly. Without this conversion a Retina primary drives a 1x
/// secondary at half the physical cursor speed — the mouse "feels slow" on the other screen.
///
/// The result is clamped so a peer reporting a bogus scale (0, or a wild value) can never make
/// the cursor teleport or freeze.
pub fn motion_scale_ratio(l: &Layout, remote: &str, loc: Option<(f64, f64)>) -> f64 {
    let own = l.local_scale_at(loc).max(0.01) as f64;
    let theirs = l.scale_of_host(remote).max(0.01) as f64;
    let auto = (own / theirs).clamp(MIN_SCALE_RATIO, MAX_SCALE_RATIO);
    effective_ratio(auto, motion_scale())
}

/// Combine the automatic ratio with the user's manual trim. Pure (no globals) so tests can
/// exercise the clamping without racing other tests over `MOTION_SCALE`.
fn effective_ratio(auto: f64, manual: f32) -> f64 {
    // The manual multiplier is a trim on top of the automatic value, not a replacement: the
    // machines' densities can change (new monitor, different Windows scaling) and the auto
    // part keeps tracking that, while the user only corrects the residual "feel".
    let m = if manual.is_finite() {
        manual as f64
    } else {
        1.0
    };
    (auto * m).clamp(MIN_EFFECTIVE_RATIO, MAX_EFFECTIVE_RATIO)
}

/// Begin forwarding control to the machine `cross.host` (landing on `cross.panel`). Hides the
/// local cursor, parks it at the shared edge, and tells the secondary the cursor is entering (so it
/// can seed its own virtual cursor).
fn enter_forwarding(ctx: &GrabCtx, c: &mut Ctrl, l: &Layout, cross: &Crossing, loc: (f64, f64)) {
    // Park on the *display the cursor is leaving*, not on the union bounding box: on a
    // multi-monitor Mac the bbox can be much taller than the screen you crossed from, and
    // parking by bbox fraction drops the cursor into dead space (or onto another display)
    // when control comes back.
    let (bl, bt, br, bb) = match local_screen_at(l, loc.0, loc.1) {
        Some(s) => screen_rect(s),
        None => match l.local_bbox() {
            Some(b) => b,
            None => return,
        },
    };
    let fx = ((loc.0 - bl) / (br - bl)).clamp(0.0, 1.0);
    let fy = ((loc.1 - bt) / (bb - bt)).clamp(0.0, 1.0);
    let park = match cross.side {
        Side::Right => (br - 1.0, bt + fy * (bb - bt)),
        Side::Left => (bl + 1.0, bt + fy * (bb - bt)),
        Side::Bottom => (bl + fx * (br - bl), bb - 1.0),
        Side::Top => (bl + fx * (br - bl), bt + 1.0),
    };
    c.remote = Some(RemoteCtrl {
        name: cross.host.clone(),
        panel: Some(cross.panel.clone()),
        side: cross.side,
        vx: park.0,
        vy: park.1,
        scale_ratio: motion_scale_ratio(l, &cross.host, Some(loc)),
    });
    c.parked = Some(park);
    c.parked_rect = Some((bl, bt, br, bb));
    c.held_keys.clear();
    c.held_buttons.clear();
    c.cooldown_until = None;
    *ctx.mode.lock().unwrap() = CaptureMode::Forwarding(cross.host.clone());
    // Detach the hardware mouse and hide the cursor *before* announcing the hand-off, so there is
    // no window in which the local pointer can still be dragged off the park point.
    crate::capture::enter_forwarding_grab();
    ctx.net.lock().unwrap().send_to(
        &cross.host,
        Message::EnterScreen {
            side: cross.side,
            fx,
            fy,
            panel: Some(cross.panel.clone()),
        },
    );
    log::info!("control handed to {} ({:?})", cross.host, cross.side);
    if let Some(r) = c.remote.as_ref() {
        crate::diag::log(&format!(
            "HAND-OFF -> {} panel={} side={:?} park=({:.0},{:.0}) scale_ratio={:.2}",
            r.name, cross.panel, cross.side, r.vx, r.vy, r.scale_ratio
        ));
    }
}

/// Return control to the local machine: release any held keys/buttons on the remote, show the local
/// cursor again, put it back exactly where it left, and tell the secondary control left.
fn leave_forwarding(ctx: &GrabCtx, c: &mut Ctrl, l: &Layout, name: &str, loc: Option<(f64, f64)>) {
    // Release held keys/buttons on the remote so it never keeps a "ghost" modifier down.
    // Queued rather than sent under the `net` lock: this runs on the event-tap thread whenever
    // the tap is disabled mid-forward, which is exactly when we must not stall on a lock.
    for k in c.held_keys.drain(..) {
        forward_to(ctx, name, InputEvent::KeyUp { key: k });
    }
    for b in c.held_buttons.drain(..) {
        forward_to(ctx, name, InputEvent::MouseUp { button: b });
    }
    let side = c.remote.as_ref().map(|r| r.side);
    // The rect of the panel we left. Returning relative to *it* (rather than to the machine's
    // bounding box) is what keeps the cursor on the display the user actually left from when the
    // Mac has several monitors of different heights.
    let rect = c.parked_rect;
    c.remote = None;
    c.parked = None;
    c.parked_rect = None;
    *ctx.mode.lock().unwrap() = CaptureMode::Local;
    c.cooldown_until =
        Some(std::time::Instant::now() + std::time::Duration::from_millis(RETURN_COOLDOWN_MS));
    if let Some(side) = side {
        let (bl, bt, br, bb) = rect
            .or_else(|| l.local_bbox())
            .unwrap_or((0.0, 0.0, 0.0, 0.0));
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
        // Re-attach the hardware mouse first, then put the cursor back on the shared edge.
        //
        // The old code skipped the warp entirely while the grab tap was active, on the theory that
        // the cursor "cannot have moved". That theory only holds while every synthetic event is
        // dropped — and it is false exactly when it matters: after an App Nap stall or a
        // `TapDisabledByTimeout` the OS has been moving the real cursor again, so control came back
        // with the arrow somewhere unrelated (the "the cursor slides around on both screens"
        // report). Warping is safe now: our own synthetic moves are tagged and ignored by the tap,
        // and the 700 ms cooldown covers any residue.
        crate::capture::leave_forwarding_grab(target);
        c.last_real = target;
    }
    ctx.net.lock().unwrap().send_to(name, Message::LeaveScreen);
    log::info!("control returned to {}", ctx.primary_name);
    crate::diag::log(&format!("RETURN <- {}", name));
}

/// Which outer edge of the local bbox a machine's panel group is attached just beyond?
fn attached_side(l: &Layout, host: &str, bbox: (f64, f64, f64, f64)) -> Option<Side> {
    let (bl, bt, br, bb) = bbox;
    let (sl, st, sr, sb) = l.host_bbox(host)?;
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

/// The secondary asked for control back (its cursor was pushed back across the shared edge).
/// Return to local if we are currently forwarding. Unlike `cycle_control`, this always returns to
/// the primary rather than rotating to the next machine.
pub fn return_control(ctx: &GrabCtx) {
    let mut c = ctx.ctrl.lock().unwrap();
    // Deliberately *not* `ctx.layout.lock()`: this runs on the event-tap thread — the tap calls
    // it the moment the OS disables the tap — and blocking there on a lock the GUI thread may be
    // holding is what turns one momentary timeout into sustained, visible lag (and can disable
    // the tap again, which is how a brief hiccup becomes "it stutters and both cursors move").
    // `layout_snap` is the lock-free copy kept for exactly this path.
    let snap = c.layout_snap.clone();
    if let Some(r) = c.remote.clone() {
        let loc = c.parked;
        leave_forwarding(ctx, &mut c, &snap, &r.name, loc);
    }
}

/// Tap-safe [`return_control`]: gives up instead of blocking when `ctrl` is held.
///
/// Called from the event-tap callback the instant macOS disables the tap. That callback is
/// already in the OS's bad books — waiting on a lock here keeps it unresponsive for longer and
/// earns another disable, which is how one hiccup turns into sustained "both cursors move".
/// Skipping the recovery is safe: the next timeout, or the user simply moving back, rights it.
pub fn try_return_control(ctx: &GrabCtx) {
    let Ok(mut c) = ctx.ctrl.try_lock() else {
        crate::diag::log("TAP DISABLED mid-forward — ctrl busy, deferred return");
        return;
    };
    let snap = c.layout_snap.clone();
    if let Some(r) = c.remote.clone() {
        let loc = c.parked;
        leave_forwarding(ctx, &mut c, &snap, &r.name, loc);
    }
}

/// Rotate control: local → each secondary → back to local. Invoked by the hotkey on the primary.
pub fn cycle_control(ctx: &GrabCtx) {
    let mut c = ctx.ctrl.lock().unwrap();
    // Same lock-free layout as `return_control`: the hotkey is detected inside the event-tap
    // callback, so this also runs on the tap thread.
    let snap = c.layout_snap.clone();
    let l: &Layout = &snap;
    if l.screens.len() <= 1 {
        return;
    }
    let Some(bbox) = l.local_bbox() else { return };
    // Rotate over *machines*, not panels: a two-monitor client is still one stop on the ring.
    let mut remotes: Vec<String> = Vec::new();
    for s in l.screens.iter().filter(|s| !s.is_local) {
        let h = s.host().to_string();
        if !remotes.contains(&h) {
            remotes.push(h);
        }
    }
    if remotes.is_empty() {
        return;
    }
    let idx = match &c.remote {
        Some(r) => remotes
            .iter()
            .position(|h| h == &r.name)
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
    let host = remotes[idx].clone();
    let side = attached_side(&l, &host, bbox).unwrap_or(Side::Right);
    let panel = entry_panel(&l, &host, side).unwrap_or_else(|| host.clone());
    let cross = Crossing { side, host, panel };
    let loc = crate::input::cursor_position().unwrap_or((0.0, 0.0));
    enter_forwarding(ctx, &mut c, &l, &cross, loc);
}

// ---- Secondary side: receiving control ----

/// Where a secondary seeds its virtual cursor when control arrives: on the display that faces
/// the primary, at the same fraction along the shared edge.
///
/// When the hub named the panel it attached the cursor to (it knows, having decided the crossing),
/// seed on exactly that display. Falling back to the bounding box is only correct for a
/// single-monitor client: with several panels the bbox fraction can land in dead space between
/// them, or on the wrong panel entirely, and the cursor then appears to "jump" on entry.
fn seed_position(
    screens: &[Screen],
    bbox: Option<(f64, f64, f64, f64)>,
    side: Side,
    fx: f64,
    fy: f64,
    panel: Option<&str>,
) -> (f64, f64) {
    if let Some(p) = panel.and_then(|n| screens.iter().find(|s| s.name == n)) {
        let (pl, pt, pr, pb) = screen_rect(p);
        let (tx, ty) = match side {
            Side::Right => (pr - 2.0, pt + fy * (pb - pt)),
            Side::Left => (pl + 2.0, pt + fy * (pb - pt)),
            Side::Bottom => (pl + fx * (pr - pl), pb - 2.0),
            Side::Top => (pl + fx * (pr - pl), pt + 2.0),
        };
        return clamp_to_screens(screens, tx, ty);
    }
    let Some((bl, bt, br, bb)) = bbox else {
        return (0.0, 0.0);
    };
    let (tx, ty) = match side {
        Side::Right => (br - 2.0, bt + fy * (bb - bt)),
        Side::Left => (bl + 2.0, bt + fy * (bb - bt)),
        Side::Bottom => (bl + fx * (br - bl), bb - 2.0),
        Side::Top => (bl + fx * (br - bl), bt + 2.0),
    };
    clamp_to_screens(screens, tx, ty)
}

/// Pull `(x, y)` into the nearest local display (or leave it alone when we know of none).
fn clamp_to_screens(screens: &[Screen], x: f64, y: f64) -> (f64, f64) {
    let mut best: Option<&Screen> = None;
    let mut best_d = f64::MAX;
    for s in screens {
        let (a, b, c, d) = screen_rect(s);
        let dx = (a - x).max(0.0).max(x - c);
        let dy = (b - y).max(0.0).max(y - d);
        let dist = dx * dx + dy * dy;
        if dist < best_d {
            best_d = dist;
            best = Some(s);
        }
    }
    match best {
        Some(s) => {
            let (a, b, c, d) = screen_rect(s);
            (x.clamp(a, c - 1.0), y.clamp(b, d - 1.0))
        }
        None => (x, y),
    }
}

/// Advance the secondary's virtual cursor by `(dx, dy)`.
///
/// Inside a display the motion is free; when it would leave that display we land on whichever
/// *other* local display is closest — that is what lets a secondary with several monitors be
/// driven across all of them. Handing control back is a separate decision: it only happens at
/// the outermost edge facing the primary (see `crossing_back`).
fn step_local(
    screens: &[Screen],
    bbox: Option<(f64, f64, f64, f64)>,
    vx: f64,
    vy: f64,
    dx: f64,
    dy: f64,
) -> (f64, f64) {
    let (nx, ny) = (vx + dx, vy + dy);
    if !screens.is_empty() {
        if let Some(s) = screens.iter().find(|s| s.contains(vx, vy)) {
            let (a, b, c, d) = screen_rect(s);
            let cx = nx.clamp(a, c - 1.0);
            let cy = ny.clamp(b, d - 1.0);
            if (nx - cx).abs() < 0.5 && (ny - cy).abs() < 0.5 {
                return (cx, cy);
            }
        }
        return clamp_to_screens(screens, nx, ny);
    }
    if let Some((a, b, c, d)) = bbox {
        return (nx.clamp(a, c - 1.0), ny.clamp(b, d - 1.0));
    }
    (nx, ny)
}

/// The primary says the cursor is entering our screen. Seed our virtual cursor at the edge facing
/// the primary and hide our real cursor.
pub fn on_enter_screen(ctx: &GrabCtx, side: Side, fx: f64, fy: f64, panel: Option<String>) {
    let mut c = ctx.ctrl.lock().unwrap();
    let Some(bbox) = c.local_bbox else { return };
    let eside = side.opposite();
    let (vx, vy) = seed_position(
        &c.local_screens,
        Some(bbox),
        eside,
        fx,
        fy,
        panel.as_deref(),
    );
    c.remote = Some(RemoteCtrl {
        name: ctx.primary_name.clone(),
        panel: panel.clone(),
        side: eside,
        vx,
        vy,
        // Unused on this side (we receive already-normalised deltas), but keep it valid.
        scale_ratio: 1.0,
    });
    drop(c);
    crate::capture::hide_cursor();
    crate::diag::log(&format!(
        "ENTER <- primary side={:?} seed=({:.0},{:.0}) panel={:?}",
        eside, vx, vy, panel
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
/// When the cursor is pushed back across the edge facing the primary, we send `ReturnControl` so
/// the primary hands control back (the primary can't tell on its own — its cursor is frozen).
pub fn on_secondary_input(ctx: &GrabCtx, ev: InputEvent) {
    let mut c = ctx.ctrl.lock().unwrap();
    let bbox = c.local_bbox;
    // Snapshot the display list before borrowing `remote` mutably (the compiler cannot split
    // the borrow through the `Option`). It is one or two small structs per event.
    let screens: Vec<Screen> = c.local_screens.clone();
    let Some(r) = c.remote.as_mut() else {
        return; // not being driven — ignore stray events
    };
    match ev {
        InputEvent::MouseMotion { dx, dy } => {
            if crossing_back(r, &screens, dx, dy) {
                // Hand control back. Capture what we need, then drop the ctrl lock before
                // the network send so we don't hold it across the socket.
                let side = r.side;
                let (vx, vy) = (r.vx, r.vy);
                drop(c);
                ctx.net.lock().unwrap().send_message(Message::ReturnControl);
                crate::diag::log(&format!(
                    "EDGE-RETURN side={:?} v=({:.0},{:.0}) d=({:.0},{:.0})",
                    side, vx, vy, dx, dy
                ));
                return;
            }
            let (nx, ny) = step_local(&screens, bbox, r.vx, r.vy, dx, dy);
            r.vx = nx;
            r.vy = ny;
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
            host: "A".into(),
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
            host: name.into(),
            ox,
            oy,
            w,
            h,
            is_local: false,
            scale: 1.0,
        }
    }

    const BBOX: (f64, f64, f64, f64) = (0.0, 0.0, 1920.0, 1080.0);

    /// A single-display machine reporting itself as one panel.
    fn one_screen() -> Vec<Screen> {
        vec![local()]
    }

    /// Shorthand for the expected `Crossing`: a single-panel machine named `name`.
    fn crossing(side: Side, name: &str) -> Crossing {
        Crossing {
            side,
            host: name.to_string(),
            panel: name.to_string(),
        }
    }

    #[test]
    fn predict_cross_right() {
        let l = Layout {
            screens: vec![local(), remote("B", 1920, 0, 1920, 1080)],
        };
        // Cursor at the right edge moving right -> cross into B.
        assert_eq!(
            predict_cross(&l, (1918.0, 540.0), (10.0, 0.0)),
            Some(crossing(Side::Right, "B"))
        );
        // Not reaching the edge -> no cross.
        assert_eq!(predict_cross(&l, (1000.0, 540.0), (10.0, 0.0)), None);
        // Moving back inside -> no cross.
        assert_eq!(predict_cross(&l, (1918.0, 540.0), (-10.0, 0.0)), None);
    }

    #[test]
    fn predict_cross_left() {
        let l = Layout {
            screens: vec![local(), remote("L", -1920, 0, 1920, 1080)],
        };
        assert_eq!(
            predict_cross(&l, (5.0, 540.0), (-10.0, 0.0)),
            Some(crossing(Side::Left, "L"))
        );
    }

    #[test]
    fn predict_cross_bottom() {
        let l = Layout {
            screens: vec![local(), remote("D", 0, 1080, 1920, 1080)],
        };
        assert_eq!(
            predict_cross(&l, (960.0, 1075.0), (0.0, 10.0)),
            Some(crossing(Side::Bottom, "D"))
        );
    }

    #[test]
    fn predict_cross_top() {
        let l = Layout {
            screens: vec![local(), remote("U", 0, -1080, 1920, 1080)],
        };
        assert_eq!(
            predict_cross(&l, (960.0, 5.0), (0.0, -10.0)),
            Some(crossing(Side::Top, "U"))
        );
    }

    #[test]
    fn predict_cross_only_remote_neighbours() {
        // No remote screen attached -> never cross, even at the edge.
        let l = Layout {
            screens: vec![local()],
        };
        assert_eq!(predict_cross(&l, (1918.0, 540.0), (10.0, 0.0)), None);
    }

    #[test]
    fn predict_cross_no_vertical_overlap() {
        // A right-hand remote that does NOT vertically overlap the local screen must not cross.
        let l = Layout {
            screens: vec![local(), remote("B", 1920, 2000, 1920, 1080)],
        };
        assert_eq!(predict_cross(&l, (1918.0, 540.0), (10.0, 0.0)), None);
    }

    // ---- multi-display primary: which screen the cursor leaves decides which neighbour ----

    #[test]
    fn predict_cross_multidisplay_left_screen_to_left_remote() {
        // Local screens: a wide one at (-1920..0) plus a wide one at (0..1920). A remote attached
        // to the LEFT side of the left screen (L). Cursor on the LEFT screen near its left edge.
        let l = Layout {
            screens: vec![
                Screen {
                    name: "L".into(),
                    host: "L".into(),
                    ox: -1920,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 1.0,
                },
                Screen {
                    name: "R".into(),
                    host: "R".into(),
                    ox: 0,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 1.0,
                },
                remote("L-remote", -3840, 0, 1920, 1080),
            ],
        };
        assert_eq!(
            predict_cross(&l, (-1918.0, 540.0), (-10.0, 0.0)),
            Some(crossing(Side::Left, "L-remote")),
            "must cross from the left screen, not from the right one"
        );
    }

    #[test]
    fn predict_cross_multidisplay_right_screen_to_right_remote() {
        let l = Layout {
            screens: vec![
                Screen {
                    name: "L".into(),
                    host: "L".into(),
                    ox: -1920,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 1.0,
                },
                Screen {
                    name: "R".into(),
                    host: "R".into(),
                    ox: 0,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 1.0,
                },
                remote("R-remote", 1920, 0, 1920, 1080),
            ],
        };
        assert_eq!(
            predict_cross(&l, (1918.0, 540.0), (10.0, 0.0)),
            Some(crossing(Side::Right, "R-remote")),
            "must cross from the right screen to the right-attached remote"
        );
    }

    // ---- crossing_back: the return decision lives on the secondary ----

    #[test]
    fn crossing_back_left_edge_returns() {
        // Secondary entered on its LEFT edge (primary to its left). Cursor mid-screen moving left
        // must NOT return; only when it reaches the left edge and keeps pushing left.
        let r = RemoteCtrl {
            name: "B".into(),
            panel: Some("A".into()),
            side: Side::Left,
            vx: 500.0,
            vy: 540.0,
            scale_ratio: 1.0,
        };
        assert!(
            !crossing_back(&r, &one_screen(), -50.0, 0.0),
            "mid-screen left move must not return"
        );
        assert!(
            !crossing_back(&r, &one_screen(), 50.0, 0.0),
            "right move must not return"
        );

        let at_edge = RemoteCtrl {
            name: "B".into(),
            panel: Some("A".into()),
            side: Side::Left,
            vx: 5.0,
            vy: 540.0,
            scale_ratio: 1.0,
        };
        assert!(
            crossing_back(&at_edge, &one_screen(), -10.0, 0.0),
            "at left edge pushing left must return"
        );
        // At the edge but moving right (into the screen) must NOT return.
        assert!(!crossing_back(&at_edge, &one_screen(), 10.0, 0.0));
    }

    #[test]
    fn crossing_back_right_edge_returns() {
        // Secondary entered on its RIGHT edge (primary to its right).
        let r = RemoteCtrl {
            name: "B".into(),
            panel: Some("A".into()),
            side: Side::Right,
            vx: 1915.0,
            vy: 540.0,
            scale_ratio: 1.0,
        };
        assert!(
            crossing_back(&r, &one_screen(), 20.0, 0.0),
            "at right edge pushing right must return"
        );
        assert!(
            !crossing_back(&r, &one_screen(), -20.0, 0.0),
            "moving left (into screen) must not return"
        );
    }

    #[test]
    fn crossing_back_vertical_edges() {
        let top = RemoteCtrl {
            name: "U".into(),
            panel: Some("A".into()),
            side: Side::Top,
            vx: 960.0,
            vy: 5.0,
            scale_ratio: 1.0,
        };
        assert!(
            crossing_back(&top, &one_screen(), 0.0, -10.0),
            "at top edge pushing up must return"
        );
        assert!(!crossing_back(&top, &one_screen(), 0.0, 10.0));

        let bottom = RemoteCtrl {
            name: "D".into(),
            panel: Some("A".into()),
            side: Side::Bottom,
            vx: 960.0,
            vy: 1075.0,
            scale_ratio: 1.0,
        };
        assert!(
            crossing_back(&bottom, &one_screen(), 0.0, 10.0),
            "at bottom edge pushing down must return"
        );
        assert!(!crossing_back(&bottom, &one_screen(), 0.0, -10.0));
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

/// End-to-end control-plane tests that run the *real* cross-screen state machine over a *real* TCP
/// loopback link (primary hub + secondary client), without any GUI or input device. Injection and
/// cursor show/hide are no-ops under `cfg(test)` (see `input.rs` / `capture.rs` seams), so this
/// exercises the full message flow and state transitions headlessly.
#[cfg(test)]
mod integration {
    use super::*;
    use crate::network::{self, Net};
    use crate::protocol::{InputEvent, Message};
    use rdev::Key;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Connect a primary hub (local screen A + remote B to its right) and a secondary client
    /// (local screen B) over loopback, and return the two `GrabCtx` plus the two incoming channels
    /// (secondary's first, primary's second) so the test can observe messages in both directions.
    fn setup_pair(
        port: u16,
    ) -> (
        GrabCtx,
        GrabCtx,
        Receiver<(String, Message)>,
        Receiver<(String, Message)>,
    ) {
        let primary_layout = Layout {
            screens: vec![
                Screen {
                    name: "A".into(),
                    host: "A".into(),
                    ox: 0,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 1.0,
                },
                Screen {
                    name: "B".into(),
                    host: "B".into(),
                    ox: 1920,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: false,
                    scale: 1.0,
                },
            ],
        };
        let secondary_layout = Layout {
            screens: vec![Screen {
                name: "B".into(),
                host: "B".into(),
                ox: 0,
                oy: 0,
                w: 1920,
                h: 1080,
                is_local: true,
                scale: 1.0,
            }],
        };

        let (ptx, prx) = channel();
        let (stx, srx) = channel();
        let pla = Arc::new(Mutex::new(primary_layout));
        let sla = Arc::new(Mutex::new(secondary_layout));

        let net_primary = network::start_hub(port, ptx, pla.clone()).expect("hub starts");
        let net_secondary = Net::idle();
        let (_net_sec, sec_tx) =
            network::connect_client(&format!("127.0.0.1:{port}"), stx, net_secondary.clone())
                .expect("client connects");
        // The real secondary app sends Hello immediately after connecting; replicate that so the
        // hub learns our name and registers the peer (otherwise no messages can be routed).
        sec_tx
            .send(Message::Hello {
                name: "B".to_string(),
                width: 1920,
                height: 1080,
                scale: 1.0,
                panels: Vec::new(),
            })
            .expect("send Hello");
        std::fs::write("/tmp/mstep.txt", "after send Hello").ok();
        // Give the system a moment to settle so the secondary's writer thread has a chance to
        // spin up (otherwise it can race with our subsequent layout locks in subtle ways on
        // some platforms).
        std::thread::sleep(Duration::from_millis(50));

        // Wait until the hub has registered the secondary (handshake + peer insert).
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if net_primary.lock().unwrap().peer_count() > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            net_primary.lock().unwrap().peer_count() > 0,
            "secondary must be registered with the hub"
        );
        // The hub reader briefly locks the layout while it sends the initial Layout frame;
        // give it a moment to finish before we touch layout ourselves so the two threads
        // never race on the same mutex (without this, on some platforms the test deadlocks
        // here). The drain window below also waits for that Layout frame to arrive.
        std::thread::sleep(Duration::from_millis(50));

        let primary = GrabCtx {
            net: net_primary,
            layout: pla.clone(),
            ctrl: Arc::new(Mutex::new(Ctrl {
                // Block-scoped guards on purpose: a bare `pla.lock()` inside a struct literal
                // keeps the temporary MutexGuard alive until the *whole literal* is evaluated,
                // so the second `pla.lock()` two fields down would self-deadlock the thread.
                local_bbox: {
                    let g = pla.lock().unwrap();
                    g.local_bbox()
                },
                layout_snap: Arc::new(pla.lock().unwrap().clone()),
                live: Arc::new(vec!["B".to_string()]),
                ..Default::default()
            })),
            mode: Mutex::new(CaptureMode::Local),
            my_name: "A".to_string(),
            primary_name: "A".to_string(),
            input_tx: None,
            ui_window_rect: Mutex::new(None),
        };
        let secondary = GrabCtx {
            net: net_secondary,
            layout: sla.clone(),
            ctrl: Arc::new(Mutex::new(Ctrl {
                local_bbox: {
                    let g = sla.lock().unwrap();
                    g.local_bbox()
                },
                layout_snap: Arc::new(sla.lock().unwrap().clone()),
                ..Default::default()
            })),
            mode: Mutex::new(CaptureMode::Local),
            my_name: "B".to_string(),
            primary_name: "A".to_string(),
            input_tx: None,
            ui_window_rect: Mutex::new(None),
        };
        (primary, secondary, srx, prx)
    }

    /// Drain the secondary's incoming channel for `dur`, applying whatever the primary sent
    /// (EnterScreen / LeaveScreen / Input) to the secondary's own control plane, and return every
    /// message that arrived (Layout snapshots are skipped).
    fn pump(sec: &GrabCtx, rx: &Receiver<(String, Message)>, dur: Duration) -> Vec<Message> {
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + dur;
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            match rx.recv_timeout(remain) {
                Ok((_, msg)) => match msg {
                    Message::Layout { .. } => continue,
                    Message::EnterScreen {
                        side,
                        fx,
                        fy,
                        panel,
                    } => {
                        on_enter_screen(sec, side, fx, fy, panel.clone());
                        got.push(Message::EnterScreen {
                            side,
                            fx,
                            fy,
                            panel,
                        });
                    }
                    Message::LeaveScreen => {
                        on_leave_screen(sec);
                        got.push(Message::LeaveScreen);
                    }
                    Message::Input(ev) => {
                        on_secondary_input(sec, ev.clone());
                        got.push(Message::Input(ev));
                    }
                    other => got.push(other),
                },
                Err(RecvTimeoutError::Timeout) => break,
                Err(_) => break,
            }
        }
        got
    }

    /// Copy the current layout into `Ctrl::layout_snap` (and seed `local_screens` for both sides)
    /// so the capture callback can read it without contending on the layout mutex. The
    /// production app's background thread keeps this fresh; tests just take a one-shot
    /// snapshot after setup so they don't race with the hub reader's own layout locks.
    fn seed_layout_snap(primary: &GrabCtx, secondary: &GrabCtx) {
        let snap = primary.layout.lock().unwrap().clone();
        let screens: Vec<_> = snap
            .screens
            .iter()
            .filter(|s| s.is_local)
            .cloned()
            .collect();
        {
            let mut c = primary.ctrl.lock().unwrap();
            c.layout_snap = Arc::new(snap.clone());
            c.local_screens = screens;
        }
        let snap2 = secondary.layout.lock().unwrap().clone();
        let screens: Vec<_> = snap2
            .screens
            .iter()
            .filter(|s| s.is_local)
            .cloned()
            .collect();
        {
            let mut c = secondary.ctrl.lock().unwrap();
            c.layout_snap = Arc::new(snap2);
            c.local_screens = screens;
        }
    }

    #[test]
    fn handoff_over_tcp() {
        let (p, s, srx, _prx) = setup_pair(19211);
        seed_layout_snap(&p, &s);

        // Primary cursor near the right edge moving right -> predict crossing into B.
        let dropped = on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 540.0)),
        );
        assert!(dropped, "event must be dropped while forwarding");

        let msgs = pump(&s, &srx, Duration::from_millis(500));
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Message::EnterScreen { .. })),
            "secondary must receive EnterScreen"
        );
        assert!(
            msgs.iter().any(|m| matches!(m, Message::Input(InputEvent::MouseMotion { dx, dy }) if *dx == 100.0 && *dy == 0.0)),
            "secondary must receive the forwarded MouseMotion"
        );

        // Secondary seeds its virtual cursor on the OPPOSITE edge of the primary's side. The
        // primary crossed its Right edge into B, so B receives it on its Left edge (x = bl + 2).
        {
            let c = s.ctrl.lock().unwrap();
            let r = c.remote.as_ref().expect("secondary should be driven");
            assert_eq!(r.side, Side::Left, "secondary enters on its Left edge");
            // seed vx = bl + 2 = 2; +100 -> 102 (no clamp; br - 1 = 1919)
            assert!((r.vx - 102.0).abs() < 0.5, "vx={}", r.vx);
            // fy = 540/1080 = 0.5 -> seed vy = bt + 0.5*(bb-bt) = 540; +0 -> 540
            assert!((r.vy - 540.0).abs() < 0.5, "vy={}", r.vy);
        }

        // A leftward move while the secondary is mid-screen must NOT return control (this is the
        // regression being guarded: the primary used to snap back on any leftward delta because
        // its own cursor is frozen at the edge).
        let dropped2 = on_capture(
            &p,
            RawInput::Motion { dx: -50.0, dy: 0.0 },
            Some((1919.0, 540.0)),
        );
        assert!(
            dropped2,
            "leftward motion mid-screen must STILL be forwarded/dropped"
        );
        assert!(
            matches!(&*p.mode.lock().unwrap(), CaptureMode::Forwarding(n) if n == "B"),
            "primary must remain forwarding after a mid-screen left move"
        );
        let msgs2 = pump(&s, &srx, Duration::from_millis(300));
        assert!(
            !msgs2.iter().any(|m| matches!(m, Message::LeaveScreen)),
            "no LeaveScreen on a mid-screen left move"
        );
        // The secondary moved left from 102 -> 52, well inside the screen.
        assert!(
            (s.ctrl.lock().unwrap().remote.as_ref().unwrap().vx - 52.0).abs() < 0.5,
            "secondary cursor should track the left move"
        );

        // The secondary (or the primary, on a disconnect) asks for control back.
        return_control(&p);

        let msgs3 = pump(&s, &srx, Duration::from_millis(500));
        assert!(
            msgs3.iter().any(|m| matches!(m, Message::LeaveScreen)),
            "secondary must receive LeaveScreen"
        );
        assert!(
            p.ctrl.lock().unwrap().remote.is_none(),
            "primary remote cleared"
        );
        assert!(
            s.ctrl.lock().unwrap().remote.is_none(),
            "secondary remote cleared"
        );
        assert!(
            p.ctrl.lock().unwrap().cooldown_until.is_some(),
            "primary cooldown must be armed on return"
        );
    }

    #[test]
    fn edge_return_roundtrip() {
        let (p, s, srx, prx) = setup_pair(19215);
        seed_layout_snap(&p, &s);

        // Cross over right.
        on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 540.0)),
        );
        pump(&s, &srx, Duration::from_millis(300));
        // secondary vx = 2 + 100 = 102

        // Drive the cursor left, but not to the edge: no ReturnControl should be produced.
        on_capture(
            &p,
            RawInput::Motion { dx: -50.0, dy: 0.0 },
            Some((1919.0, 540.0)),
        );
        pump(&s, &srx, Duration::from_millis(300)); // vx 102 -> 52
        assert!(
            prx.try_recv().is_err(),
            "no ReturnControl while the secondary is still mid-screen"
        );

        // Drive left past the left edge: the secondary detects crossing-back and sends
        // ReturnControl over the real TCP loopback to the primary.
        on_capture(
            &p,
            RawInput::Motion {
                dx: -100.0,
                dy: 0.0,
            },
            Some((1919.0, 540.0)),
        );
        pump(&s, &srx, Duration::from_millis(300)); // vx 52 -> -48 -> crosses left edge

        let mut got_return = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            match prx.recv_timeout(Duration::from_millis(100)) {
                Ok((_, Message::ReturnControl)) => {
                    got_return = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        assert!(
            got_return,
            "secondary must send ReturnControl to the primary"
        );

        // The primary's message handler calls return_control -> control returns.
        return_control(&p);
        let msgs = pump(&s, &srx, Duration::from_millis(300));
        assert!(
            msgs.iter().any(|m| matches!(m, Message::LeaveScreen)),
            "secondary receives LeaveScreen after ReturnControl"
        );
        assert!(
            p.ctrl.lock().unwrap().remote.is_none(),
            "primary back to local"
        );
    }

    /// While forwarding, a click that lands inside MouseShare's own window must return control
    /// and be delivered locally instead of being swallowed and forwarded to the secondary.
    #[test]
    fn click_inside_ui_window_returns_control() {
        let (p, s, _srx, _prx) = setup_pair(19216);
        seed_layout_snap(&p, &s);

        // The GUI publishes its window rect (a small window parked near the shared edge,
        // the worst case: the parked cursor sits right on top of it).
        *p.ui_window_rect.lock().unwrap() = Some((1800.0, 0.0, 1900.0, 200.0));

        on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 100.0)),
        );
        assert!(
            matches!(*p.mode.lock().unwrap(), CaptureMode::Forwarding(ref n) if n == "B"),
            "hand-off must be active before the UI click"
        );

        // Click inside the published UI window rect: returns control, click delivered locally.
        let dropped = on_capture(
            &p,
            RawInput::ButtonDown(MsButton::Left),
            Some((1850.0, 100.0)),
        );
        assert!(
            !dropped,
            "UI-window click must be delivered locally, not forwarded"
        );
        assert!(
            matches!(*p.mode.lock().unwrap(), CaptureMode::Local),
            "control must be back to local after the UI click"
        );
        assert!(
            p.ctrl.lock().unwrap().cooldown_until.is_some(),
            "return arms the re-cross cooldown"
        );

        // A click outside the window while forwarding is still forwarded (dropped locally).
        // Wait out the re-cross cooldown first — it exists precisely to block an instant
        // re-hand-off after returning.
        std::thread::sleep(Duration::from_millis(750));
        on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 500.0)),
        );
        assert!(
            matches!(*p.mode.lock().unwrap(), CaptureMode::Forwarding(ref n) if n == "B"),
            "hand-off re-engaged away from the UI window"
        );
        let dropped = on_capture(
            &p,
            RawInput::ButtonDown(MsButton::Left),
            Some((500.0, 500.0)),
        );
        assert!(
            dropped,
            "ordinary clicks stay forwarded while a secondary has control"
        );
    }

    #[test]
    fn cooldown_blocks_recross() {
        let (p, s, srx, _prx) = setup_pair(19212);
        seed_layout_snap(&p, &s);

        on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 540.0)),
        );
        pump(&s, &srx, Duration::from_millis(300));
        return_control(&p);
        pump(&s, &srx, Duration::from_millis(300));
        assert!(
            p.ctrl.lock().unwrap().cooldown_until.is_some(),
            "cooldown armed after return"
        );

        // Immediate re-cross attempt must be suppressed by the cooldown.
        let dropped = on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 540.0)),
        );
        assert!(!dropped, "cooldown must suppress immediate re-handoff");
        assert!(
            p.ctrl.lock().unwrap().remote.is_none(),
            "still local during cooldown"
        );
        let msgs = pump(&s, &srx, Duration::from_millis(300));
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, Message::EnterScreen { .. })),
            "no re-enter during cooldown"
        );
    }

    #[test]
    fn held_keys_released_on_leave() {
        let (p, s, srx, _prx) = setup_pair(19213);
        seed_layout_snap(&p, &s);

        on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1918.0, 540.0)),
        );
        pump(&s, &srx, Duration::from_millis(300));

        // Hold a key while forwarding.
        on_capture(&p, RawInput::KeyDown(Key::KeyA), None);

        // Hand control back -> leave_forwarding should release the held key first.
        return_control(&p);
        let msgs = pump(&s, &srx, Duration::from_millis(300));

        let keyup_idx = msgs
            .iter()
            .position(|m| matches!(m, Message::Input(InputEvent::KeyUp { key: Key::KeyA })));
        let leave_idx = msgs.iter().position(|m| matches!(m, Message::LeaveScreen));
        assert!(keyup_idx.is_some(), "remote key must be released");
        assert!(leave_idx.is_some(), "LeaveScreen must be sent");
        assert!(
            keyup_idx.unwrap() < leave_idx.unwrap(),
            "held key must be released BEFORE control leaves"
        );
    }

    #[test]
    fn hotkey_enters_first_remote() {
        let (p, s, _srx, _prx) = setup_pair(19214);
        seed_layout_snap(&p, &s);

        // ScrollLock in Local mode must rotate control into the first secondary (B).
        on_capture(&p, RawInput::KeyDown(Key::ScrollLock), None);
        assert!(
            matches!(&*p.mode.lock().unwrap(), CaptureMode::Forwarding(n) if n.as_str() == "B"),
            "first hotkey must hand control to B"
        );
        assert!(
            p.ctrl
                .lock()
                .unwrap()
                .remote
                .as_ref()
                .map(|r| r.name == "B")
                .unwrap_or(false),
            "primary remote should be B"
        );
    }

    // ---- HiDPI normalisation: forwarded deltas must land in the receiver's units ----

    #[test]
    fn motion_scale_ratio_converts_between_scales() {
        let l = Layout {
            screens: vec![
                Screen {
                    name: "A".into(),
                    host: "A".into(),
                    ox: 0,
                    oy: 0,
                    w: 1470,
                    h: 956,
                    is_local: true,
                    scale: 2.0,
                },
                Screen {
                    name: "B".into(),
                    host: "B".into(),
                    ox: 1470,
                    oy: 0,
                    w: 3072,
                    h: 1920,
                    is_local: false,
                    scale: 1.0,
                },
            ],
        };
        // A Retina primary (2.0) driving a 1x secondary must send twice the delta.
        assert!(
            (motion_scale_ratio(&l, "B", Some((1460.0, 500.0))) - 2.0).abs() < 1e-6,
            "retina -> 1x must double the delta"
        );
        // No location: fall back to the first local screen (still 2.0 here).
        assert!((motion_scale_ratio(&l, "B", None) - 2.0).abs() < 1e-6);
        // Same scale on both sides -> no conversion.
        assert!((motion_scale_ratio(&l, "A", Some((10.0, 10.0))) - 1.0).abs() < 1e-6);
        // Unknown peer -> treated as 1x.
        assert!((motion_scale_ratio(&l, "nope", Some((10.0, 10.0))) - 2.0).abs() < 1e-6);
        // A bogus 0 scale must be clamped, not blow up into infinity/NaN.
        let bogus = Layout {
            screens: vec![Screen {
                name: "Z".into(),
                host: "Z".into(),
                ox: 0,
                oy: 0,
                w: 100,
                h: 100,
                is_local: false,
                scale: 0.0,
            }],
        };
        let r = motion_scale_ratio(&bogus, "Z", None);
        assert!(
            r.is_finite() && r <= MAX_SCALE_RATIO,
            "ratio must stay bounded, got {r}"
        );
    }

    #[test]
    fn manual_trim_scales_and_stays_bounded() {
        // Default trim leaves the automatic ratio untouched.
        assert!((effective_ratio(2.0, 1.0) - 2.0).abs() < 1e-6);
        // Halve / double the auto value.
        assert!((effective_ratio(2.0, 0.5) - 1.0).abs() < 1e-6);
        assert!((effective_ratio(2.0, 1.5) - 3.0).abs() < 1e-6);
        // Absurd input must be clamped instead of teleporting the cursor.
        assert_eq!(effective_ratio(2.0, 100.0), MAX_EFFECTIVE_RATIO);
        assert_eq!(effective_ratio(2.0, 0.0), MIN_EFFECTIVE_RATIO);
        assert_eq!(
            effective_ratio(2.0, f32::NAN),
            2.0,
            "NaN must fall back to 1.0"
        );
    }

    #[test]
    fn hidpi_primary_doubles_forwarded_delta() {
        let (p, s, srx, _prx) = setup_hidpi_pair(19217);
        seed_layout_snap(&p, &s);

        // Cross into B from the Retina primary's right edge.
        let dropped = on_capture(
            &p,
            RawInput::Motion { dx: 100.0, dy: 0.0 },
            Some((1468.0, 478.0)),
        );
        assert!(dropped, "event must be dropped while forwarding");

        let msgs = pump(&s, &srx, Duration::from_millis(500));
        let fwd = msgs.iter().find_map(|m| match m {
            Message::Input(InputEvent::MouseMotion { dx, dy }) => Some((*dx, *dy)),
            _ => None,
        });
        let (dx, dy) = fwd.expect("secondary must receive the forwarded MouseMotion");
        // 100 logical points on a @2x primary = 200 physical pixels = 200 units on a 1x peer.
        assert!(
            (dx - 200.0).abs() < 0.5,
            "dx must be scaled by 2.0, got {dx}"
        );
        assert!((dy - 0.0).abs() < 0.5, "dy={dy}");
        assert!(
            (p.ctrl.lock().unwrap().remote.as_ref().unwrap().scale_ratio - 2.0).abs() < 1e-6,
            "scale_ratio must be captured in the RemoteCtrl"
        );
    }

    /// Like `setup_pair`, but the primary's local screen is Retina (scale 2.0) while the
    /// secondary reports scale 1.0 — the Mac -> Windows case that used to halve cursor speed.
    fn setup_hidpi_pair(
        port: u16,
    ) -> (
        GrabCtx,
        GrabCtx,
        Receiver<(String, Message)>,
        Receiver<(String, Message)>,
    ) {
        let primary_layout = Layout {
            screens: vec![
                Screen {
                    name: "A".into(),
                    host: "A".into(),
                    ox: 0,
                    oy: 0,
                    w: 1470,
                    h: 956,
                    is_local: true,
                    scale: 2.0,
                },
                Screen {
                    name: "B".into(),
                    host: "B".into(),
                    ox: 1470,
                    oy: 0,
                    w: 3072,
                    h: 1920,
                    is_local: false,
                    scale: 1.0,
                },
            ],
        };
        let secondary_layout = Layout {
            screens: vec![Screen {
                name: "B".into(),
                host: "B".into(),
                ox: 0,
                oy: 0,
                w: 3072,
                h: 1920,
                is_local: true,
                scale: 1.0,
            }],
        };

        let (ptx, prx) = channel();
        let (stx, srx) = channel();
        let pla = Arc::new(Mutex::new(primary_layout));
        let sla = Arc::new(Mutex::new(secondary_layout));

        let net_primary = network::start_hub(port, ptx, pla.clone()).expect("hub starts");
        let net_secondary = Net::idle();
        let (_net_sec, sec_tx) =
            network::connect_client(&format!("127.0.0.1:{port}"), stx, net_secondary.clone())
                .expect("client connects");
        sec_tx
            .send(Message::Hello {
                name: "B".to_string(),
                width: 3072,
                height: 1920,
                scale: 1.0,
                panels: Vec::new(),
            })
            .expect("send Hello");

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if net_primary.lock().unwrap().peer_count() > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            net_primary.lock().unwrap().peer_count() > 0,
            "secondary must be registered with the hub"
        );
        // The hub reader briefly locks the layout while it sends the initial Layout frame;
        // give it a moment to finish before we touch layout ourselves so the two threads
        // never race on the same mutex (without this, on some platforms the test deadlocks
        // here). The drain window below also waits for that Layout frame to arrive.
        std::thread::sleep(Duration::from_millis(50));

        let primary = GrabCtx {
            net: net_primary,
            layout: pla.clone(),
            ctrl: Arc::new(Mutex::new(Ctrl {
                local_bbox: pla.lock().unwrap().local_bbox(),
                // layout_snap and local_screens are seeded lazily by `seed_layout_snap` after
                // setup, so creating the control plane never contends on the layout lock with
                // the hub reader (which is briefly locking it during handle_primary_conn).
                //
                // `live` must list the peer up front: `on_capture` refuses to hand control to a
                // host it has not seen in a live/reachable snapshot, so without this the
                // hidpi hand-off never happens and the forwarding assertions fail.
                live: Arc::new(vec!["B".to_string()]),
                ..Default::default()
            })),
            mode: Mutex::new(CaptureMode::Local),
            my_name: "A".to_string(),
            primary_name: "A".to_string(),
            input_tx: None,
            ui_window_rect: Mutex::new(None),
        };
        let secondary = GrabCtx {
            net: net_secondary,
            layout: sla.clone(),
            ctrl: Arc::new(Mutex::new(Ctrl {
                local_bbox: sla.lock().unwrap().local_bbox(),
                ..Default::default()
            })),
            mode: Mutex::new(CaptureMode::Local),
            my_name: "B".to_string(),
            primary_name: "A".to_string(),
            input_tx: None,
            ui_window_rect: Mutex::new(None),
        };
        (primary, secondary, srx, prx)
    }
}
