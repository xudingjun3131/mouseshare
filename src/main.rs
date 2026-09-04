//! MouseShare — share mouse / keyboard / clipboard across computers over LAN.
//!
//! Architecture
//! -----------
//! * The **primary** runs a TCP hub and captures the real input via `rdev::listen`.
//! * **Secondaries** connect to the primary and inject the forwarded input via `rdev::simulate`.
//! * A shared `Layout` (edited in the GUI) defines where each machine's screen sits in a virtual
//!   desktop; crossing an edge hands control (and the cursor) to the neighbour.
//! * Clipboard changes are broadcast and loop-suppressed.

// On Windows, build a GUI-subsystem executable (no black console window). The window then
// shows in the taskbar with the embedded logo icon instead of spawning a `cmd` console.
// Ignored on macOS/Linux (no such subsystem there).
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
// NOTE: when control is local the primary's cursor must be allowed to leave a local screen at the
// edge that has a secondary beyond it (see `handle_capture` / `outward_handoff`). The OS pins the
// real cursor onto a display, so the hand-off check runs *before* local tracking.

mod app;
mod clipboard;
mod config;
mod diag;
mod i18n;
mod input;
mod layout;
mod network;
mod protocol;

use crate::config::{load_config, save_config, Config};
use crate::i18n::Lang;
use crate::layout::Layout;
use crate::network::{connect_client, start_hub, Net};
use crate::protocol::{InputEvent, Message};
use log::info;
use rdev::{display_size, Event, EventType, Key};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

/// Throttle counter for diagnostic cursor-position samples (one per 100 motion events).
static MOTION_SAMPLES: AtomicU64 = AtomicU64::new(0);
/// Throttle counter for diagnostic samples taken while a secondary has control.
static REMOTE_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// macOS cursor visibility, used to hide the real cursor while a secondary has control.
/// This is the perceptual core of the hand-off: when control moves to the secondary, the
/// local cursor *vanishes* and the secondary's cursor appears — the same cue Synergy gives.
/// Without it, the parked cursor sitting visibly in the middle of the screen reads as
/// "crossing failed", the user reaches for the mouse, and control instantly bounces back.
#[cfg(target_os = "macos")]
mod sys_cursor {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HIDDEN: AtomicBool = AtomicBool::new(false);
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGMainDisplayID() -> u32;
    }
    pub fn hide() {
        if !HIDDEN.swap(true, Ordering::Relaxed) {
            unsafe { CGDisplayHideCursor(CGMainDisplayID()) };
        }
    }
    pub fn show() {
        if HIDDEN.swap(false, Ordering::Relaxed) {
            unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
        }
    }
}
/// Non-macOS: nothing to do — the parking cursor is only a cosmetic concern there and the
/// primary currently always runs on macOS in practice.
#[cfg(not(target_os = "macos"))]
mod sys_cursor {
    pub fn hide() {}
    pub fn show() {}
}

fn main() -> anyhow::Result<()> {
    // `--probe`: coordinate-space self-test. Warps the cursor to known points and compares
    // the positions reported by the event stream (rdev::listen) with direct Core Graphics
    // reads (CGEventGetLocation). Any mismatch between the two — or versus the layout
    // coordinates — is the #1 crossing killer, so this makes it measurable in one command.
    if std::env::args().any(|a| a == "--probe") {
        return probe();
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut config: Config = load_config();
    let my_name = config.name.clone();

    // For the primary, the address shown to secondaries must be this machine's real LAN IP,
    // not the `192.168.1.100` placeholder shipped in Config::default(). Auto-detect it on
    // startup so the GUI always displays a connectable address. (The "检测IP" button still
    // works as a manual refresh.)
    if config.mode == "primary" {
        if let Ok(ip) = local_ip_address::local_ip() {
            config.server_addr = format!("{}:{}", ip, config.port);
            save_config(&config);
        } else {
            log::warn!("could not detect local IP; keeping configured server_addr");
        }
    }

    let mode = config.mode.clone();
    let server_addr = config.server_addr.clone();
    let port = config.port;
    let primary_name = config.primary_name.clone();

    // Incoming channel: (peer name, message).
    let (inc_tx, inc_rx) = channel::<(String, Message)>();

    // Never let a networking failure kill the process silently: when launched from Finder that
    // looks exactly like "I clicked the app and nothing happened, no dialog either". Record the
    // error, fall back to an idle Net, and always open the GUI so the user can see and fix it.
    let mut startup_error: Option<String> = None;

    // Shared layout state (used by the hub to push screens to secondaries, by the incoming
    // handler to register peers, and by the capture thread for cursor hand-off).
    // The primary's layout starts from its *real* displays (so a multi-monitor Mac shows every
    // screen and the cursor roams between them natively); secondaries adopt the layout the
    // primary pushes once connected.
    let layout: Arc<Mutex<Layout>> = Arc::new(Mutex::new(if mode == "primary" {
        detect_primary_layout(&primary_name)
    } else {
        config.layout.clone()
    }));

    let net: Arc<Mutex<Net>> = if mode == "primary" {
        match start_hub(port, inc_tx.clone(), layout.clone()) {
            Ok(n) => n,
            Err(e) => {
                // Error text follows the UI language chosen in the config.
                let msg = Lang::from_code(&config.lang).listen_fail(port, e);
                log::error!("{}", msg);
                startup_error = Some(msg);
                Net::idle()
            }
        }
    } else {
        let n = Net::idle();
        match connect_client(&server_addr, inc_tx.clone(), n.clone()) {
            Ok((net_inner, tx)) => {
                let (w, h) = display_size().unwrap_or((1920, 1080));
                tx.send(Message::Hello {
                    name: my_name.clone(),
                    width: w as u32,
                    height: h as u32,
                })
                .ok();
                net_inner
            }
            Err(e) => {
                let msg = Lang::from_code(&config.lang).connect_fail(&server_addr, e);
                log::error!("{}", msg);
                startup_error = Some(msg);
                n
            }
        }
    };

    // Control-plane state: which machine currently "has" the mouse (local or a remote
    // secondary), the parked real-cursor position, and the pinned-edge push detector.
    let ctrl: Arc<Mutex<Ctrl>> = Arc::new(Mutex::new(Ctrl::default()));

    // ---- Startup diagnostics dump (file-based; stderr is invisible when launched from Finder) ----
    {
        let l = layout.lock().unwrap();
        let screens = l
            .screens
            .iter()
            .map(|s| {
                format!(
                    "{}({}x{}@{},{} local={})",
                    s.name, s.w, s.h, s.ox, s.oy, s.is_local
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        diag::log(&format!(
            "startup mode={} name={} port={} screens=[{}] bbox={:?} diag_log={}",
            mode,
            my_name,
            port,
            screens,
            l.local_bbox(),
            diag::log_path().display()
        ));
    }

    // ---- Incoming message handler ----
    {
        let net = net.clone();
        let layout = layout.clone();
        let last_seen = Arc::new(Mutex::new(String::new()));
        let mode2 = mode.clone();
        let ctrl = ctrl.clone();
        let primary_name = primary_name.clone();
        std::thread::spawn(move || {
            for (from, msg) in inc_rx {
                match msg {
                    Message::Clipboard { text } => {
                        if mode2 == "secondary" {
                            clipboard::set_clipboard(&text);
                            *last_seen.lock().unwrap() = text;
                        } else {
                            // Primary: mirror locally and relay to other secondaries.
                            clipboard::set_clipboard(&text);
                            *last_seen.lock().unwrap() = text.clone();
                            net.lock().unwrap().broadcast_clipboard(&text, Some(&from));
                        }
                    }
                    Message::Input(ev) => {
                        if mode2 == "secondary" {
                            input::apply_input(&ev);
                        }
                        // Primary originated the input; never receives it back.
                    }
                    Message::Hello { name, width, height } => {
                        // Auto-register every secondary as a screen so the client count is
                        // unbounded — no manual layout editing required to add more machines.
                        if mode2 == "primary" {
                            if layout.lock().unwrap().ensure_screen(&name, width, height, false) {
                                info!("auto-registered screen for peer {}", name);
                            }
                        }
                    }
                    Message::Layout { layout: new_layout } => {
                        // The primary pushes its full layout to secondaries so every machine
                        // draws the same map. Secondaries adopt it verbatim; the primary never
                        // receives this message.
                        if mode2 == "secondary" {
                            *layout.lock().unwrap() = new_layout;
                        }
                    }
                    Message::Hotkey => {
                        // A secondary pressed the switch hotkey; only the primary actually rotates
                        // control (it owns the cursor and the layout).
                        if mode2 == "primary" {
                            cycle_control(&net, &layout, &ctrl, &primary_name);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // ---- Clipboard monitor (both roles) ----
    {
        let net = net.clone();
        let last_seen = Arc::new(Mutex::new(String::new()));
        let mode2 = mode.clone();
        clipboard::start_monitor(last_seen, move |text: String| {
            net.lock().unwrap().broadcast_clipboard(&text, None);
            let _ = &mode2;
        });
    }

    // ---- Capture ----
    if mode == "primary" {
        {
            let net = net.clone();
            let layout = layout.clone();
            let ctrl = ctrl.clone();
            let primary_name = primary_name.clone();
            input::start_capture(move |event: Event| {
                handle_capture(event, &net, &layout, &ctrl, &primary_name);
            });
        }
        // ---- Edge-rest poller (primary only) ----
        // A second, event-independent crossing trigger. Samples the cursor position straight
        // from the OS: if it rests inside a shared-edge pin zone (position unchanged — which
        // is exactly what a pinned cursor does) for EDGE_REST_MS while control is local,
        // control is handed to the secondary beyond that edge. This works even when the
        // event stream is silent at the edge, which is what made crossing unreliable before.
        {
            let net = net.clone();
            let layout = layout.clone();
            let ctrl = ctrl.clone();
            std::thread::spawn(move || {
                let mut last_pos: Option<(f64, f64)> = None;
                let mut rest_since: Option<std::time::Instant> = None;
                let mut reported = false;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
                    let Some((x, y)) = input::cursor_position() else {
                        return; // no OS sampler on this platform
                    };
                    // Cursor moved? reset the rest clock.
                    if let Some((px, py)) = last_pos {
                        if (x - px).abs() > 0.5 || (y - py).abs() > 0.5 {
                            rest_since = None;
                            reported = false;
                        }
                    }
                    last_pos = Some((x, y));

                    // Only meaningful while control is local (and outside the just-returned
                    // grace window — the returned cursor parks inside the shared-edge pin
                    // zone, so the poller would otherwise re-cross immediately).
                    let blocked = {
                        let c = ctrl.lock().unwrap();
                        c.remote.is_some()
                            || c.cooldown_until
                                .map(|t| std::time::Instant::now() < t)
                                .unwrap_or(false)
                    };
                    if blocked {
                        rest_since = None;
                        continue;
                    }
                    let (hit, bbox) = {
                        let l = layout.lock().unwrap();
                        if l.screens.len() <= 1 {
                            continue;
                        }
                        let Some(bbox) = l.local_bbox() else { continue };
                        let hit = edge_remote(l.screens.iter().filter(|s| !s.is_local), bbox, x, y);
                        (hit, bbox)
                    };
                    let Some((side, name)) = hit else {
                        rest_since = None;
                        reported = false;
                        continue;
                    };
                    let since = *rest_since.get_or_insert_with(std::time::Instant::now);
                    let rested = since.elapsed().as_millis();
                    // Throttled diagnostics: one line when the cursor first comes to rest in a
                    // pin zone, then one per second while it stays there.
                    if !reported {
                        reported = true;
                        diag::log(&format!(
                            "poller: cursor at rest in {:?} pin zone at ({:.0},{:.0}) bbox={:?} target={} rest_ms={}",
                            side, x, y, bbox, name, rested
                        ));
                    }
                    if rested >= EDGE_REST_MS {
                        diag::log(&format!(
                            "poller: handing control to {} ({:?}) after {}ms rest at ({:.0},{:.0})",
                            name, side, rested, x, y
                        ));
                        let mut c = ctrl.lock().unwrap();
                        let l = layout.lock().unwrap();
                        hand_off(&mut c, &l, &net, side, &name, x, y, bbox);
                        rest_since = None;
                        reported = false;
                    }
                }
            });
        }
    } else {
        info!("running as secondary; waiting for input from {}", server_addr);
        // Secondaries also listen for the switch hotkey (ScrollLock, or Ctrl+Alt+Space — most
        // Mac keyboards have no ScrollLock key) so the user can hand control back to the primary
        // from the Windows side — the press is forwarded to the primary, which rotates ownership.
        let net_hk = net.clone();
        let hk = Arc::new(Mutex::new(HotkeyState::default()));
        input::start_capture(move |e: Event| {
            let (k, down) = match e.event_type {
                EventType::KeyPress(k) => (k, true),
                EventType::KeyRelease(k) => (k, false),
                _ => return,
            };
            if hotkey_fired(k, down, &mut hk.lock().unwrap()) {
                net_hk.lock().unwrap().send_message(Message::Hotkey);
            }
        });
    }

    // ---- GUI on the main thread ----
    let gui_app = app::MouseShareApp::new(
        config,
        layout,
        net,
        my_name,
        startup_error,
        inc_tx.clone(),
        ctrl.clone(),
    );

    // Window icon: the bundled mouse logo. Without this a bare (non-.app) binary shows the
    // generic executable icon in the Dock / title bar; the .app bundle still gets AppIcon.icns
    // from the CI packaging step, so this only makes the dev/preview build match the release.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../resources/mouse-logo.png"))
        .map(Arc::new)
        .ok();

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1120.0, 720.0])
        .with_min_inner_size([860.0, 560.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        "MouseShare",
        options,
        Box::new(move |cc| {
            // Install a CJK fallback font BEFORE the GUI starts drawing, so the very first frame
            // already shows Chinese correctly (otherwise first frame is tofu, then it gets
            // replaced on the next frame).
            app::setup_fonts(&cc.egui_ctx);
            app::setup_style(&cc.egui_ctx);
            Ok::<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>>(Box::new(gui_app))
        }),
    );
    if let Err(e) = result {
        log::error!("gui error: {}", e);
    }
    Ok(())
}

// ---- Control plane: which machine "has" the mouse ----------------------------------
//
// A two-state machine (Synergy-style):
//
// * **Local** — the real cursor is free on this machine. NOTHING is ever forwarded, so a
//   second cursor can never move on another machine while this one moves (the old
//   "both move at once" bug is impossible by construction). We only watch for the cursor
//   being *pinned* against an outer edge of the local bounding box that has a secondary
//   attached just beyond it. The OS clamps the cursor at a display edge, so a continuous
//   outward push arrives as an event *at* the edge with an outward delta — that single
//   pinned push (`PIN_THRESHOLD = 1`) hands control to the secondary right away: crossing
//   is permanently on, no repeated push-jamming required.
//
// * **Remote** — a secondary has control. Every motion delta is forwarded to it as an
//   absolute position inside its own screen, and the real cursor is re-centred on this
//   machine whenever it drifts, so nothing visibly moves here. Moving back across the
//   shared edge inside the secondary's screen returns control to this machine.

/// Within this distance of the bbox edge the cursor counts as pinned against it.
const EDGE_PIN: f64 = 3.0;
/// A remote counts as attached just beyond an edge when its gap is within this distance.
/// Generous on purpose: tiles dragged in the canvas only line up roughly (small up/down/
/// left/right offsets), and crossing must stay permanently on regardless. The hand-off only
/// needs to know which neighbour lies beyond the edge — the exact gap is irrelevant.
const EDGE_ATTACH: f64 = 240.0;
/// After a pin, pull the cursor this far back inside so the next push produces fresh events.
const BOUNCE_IN: f64 = 12.0;
/// Outward pushes needed (within `PIN_WINDOW_MS`) before control is handed off.
/// 1 = cross on the first push into a shared edge — crossing is always on.
const PIN_THRESHOLD: u32 = 1;
/// Pushes inside this time window accumulate toward the hand-off.
const PIN_WINDOW_MS: u128 = 900;
/// Max drift of the parked real cursor from the anchor before it is re-centred.
const PARK_SLACK: f64 = 40.0;
/// How often the edge-rest poller samples the cursor position.
const POLL_INTERVAL_MS: u64 = 50;
/// How long the cursor must REST inside a shared-edge pin zone (position unchanged) before
/// the poller hands control to the secondary beyond it. This trigger does not depend on the
/// event stream at all: when the OS pins the cursor at a display edge it may deliver no
/// motion events (or only zero-delta echoes) — both are invisible to the event path, but a
/// direct OS position sample still shows the cursor sitting in the pin zone.
const EDGE_REST_MS: u128 = 400;
/// After control returns to the primary, automatic hand-offs are suppressed for this long.
/// The returned cursor parks just inside the shared edge, so without this a stray push or
/// a glide along that edge re-crosses immediately and control ping-pongs between machines.
const RETURN_COOLDOWN_MS: u64 = 700;

/// Which side of the local bounding box a secondary is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Right,
    Left,
    Top,
    Bottom,
}

/// Control of a remote secondary: its name, the shared edge, and the virtual cursor position
/// in the secondary's own local coordinates (origin = top-left of its screen).
#[derive(Debug, Clone)]
struct RemoteCtrl {
    name: String,
    side: Side,
    vx: f64,
    vy: f64,
}

/// Mutable control-plane state shared between the capture thread and the hotkey paths.
/// Lock order: `ctrl` first, then `layout`, then `net` — never the other way around.
#[derive(Debug, Default)]
struct Ctrl {
    init: bool,
    last_real: (f64, f64),
    /// `Some` while a secondary has control (cursor handed off); `None` = local control.
    remote: Option<RemoteCtrl>,
    /// Consecutive outward pushes at a pinned edge (drives the automatic hand-off).
    pins: u32,
    last_pin: Option<std::time::Instant>,
    /// Suppress automatic hand-offs until this instant — set right after control returns,
    /// because the returned cursor is parked just inside the shared edge and any stray push
    /// along that edge would otherwise re-cross immediately (a hand-off/return ping-pong).
    cooldown_until: Option<std::time::Instant>,
    /// Modifier/hotkey bookkeeping for the switch hotkey.
    hk: HotkeyState,
}

/// State for the switch-hotkey detector: which modifiers are currently held.
#[derive(Debug, Default)]
struct HotkeyState {
    ctrl: bool,
    alt: bool,
}

/// Switch hotkey: **ScrollLock** (kept for compatibility) or **Ctrl+Alt+Space**.
///
/// Ctrl+Alt+Space is the primary choice because most Mac keyboards — every MacBook's built-in
/// keyboard — have no ScrollLock key at all, which made the hotkey look unimplemented there.
/// This is called for both key presses and releases so the modifier state stays in sync; it
/// returns `true` only on the press that fires the switch.
fn hotkey_fired(k: Key, down: bool, st: &mut HotkeyState) -> bool {
    match k {
        Key::ControlLeft | Key::ControlRight => st.ctrl = down,
        Key::Alt | Key::AltGr => st.alt = down,
        Key::ScrollLock => return down,
        Key::Space => return down && st.ctrl && st.alt,
        _ => {}
    }
    false
}

fn handle_capture(
    event: Event,
    net: &Arc<Mutex<Net>>,
    layout: &Arc<Mutex<Layout>>,
    ctrl: &Arc<Mutex<Ctrl>>,
    primary_name: &str,
) {
    match event.event_type {
        EventType::MouseMove { x, y } => {
            let mut c = ctrl.lock().unwrap();
            if !c.init {
                c.init = true;
                c.last_real = (x, y);
                return;
            }
            let d = (x - c.last_real.0, y - c.last_real.1);
            c.last_real = (x, y);
            if d.0 == 0.0 && d.1 == 0.0 {
                return; // echo of our own warp — no real motion
            }
            let l = layout.lock().unwrap();
            if l.screens.len() <= 1 {
                return; // nothing to hand control to
            }
            let Some(bbox) = l.local_bbox() else { return };
            match c.remote.clone() {
                None => local_move(&mut c, l, net, bbox, x, y, d),
                Some(r) => remote_move(&mut c, l, net, bbox, primary_name, r, x, y, d),
            }
        }

        EventType::ButtonPress(b) => forward_if_remote(
            net,
            ctrl,
            InputEvent::MouseDown {
                button: input::button_to_ms(b),
            },
        ),
        EventType::ButtonRelease(b) => forward_if_remote(
            net,
            ctrl,
            InputEvent::MouseUp {
                button: input::button_to_ms(b),
            },
        ),

        EventType::Wheel { delta_x, delta_y } => forward_if_remote(
            net,
            ctrl,
            InputEvent::Wheel {
                dx: delta_x,
                dy: delta_y,
            },
        ),

        EventType::KeyPress(k) => {
            // The switch hotkey (ScrollLock, or Ctrl+Alt+Space — Mac keyboards have no
            // ScrollLock key) rotates control to the next machine.
            let fired = {
                let mut c = ctrl.lock().unwrap();
                hotkey_fired(k, true, &mut c.hk)
            };
            if fired {
                cycle_control(net, layout, ctrl, primary_name);
                return;
            }
            forward_if_remote(net, ctrl, InputEvent::KeyDown { key: k });
        }
        EventType::KeyRelease(k) => {
            // Keep modifier state in sync on releases too (a no-op for non-modifier keys).
            hotkey_fired(k, false, &mut ctrl.lock().unwrap().hk);
            // Ignore the hotkey's own release so a single tap cycles exactly once.
            if k == Key::ScrollLock {
                return;
            }
            forward_if_remote(net, ctrl, InputEvent::KeyUp { key: k });
        }
    }
}

/// Local control: watch for an outward push at an edge that has a secondary beyond it.
/// Never forwards anything — the double-motion bug is impossible by construction.
fn local_move(
    c: &mut Ctrl,
    l: std::sync::MutexGuard<'_, Layout>,
    net: &Arc<Mutex<Net>>,
    bbox: (f64, f64, f64, f64),
    x: f64,
    y: f64,
    d: (f64, f64),
) {
    // Throttled position sampling (every 100th motion event): shows whether the reported
    // cursor coordinates line up with the layout's bbox at all. A mismatch here (e.g. a
    // coordinate-space or display-arrangement discrepancy) is the #1 crossing killer.
    if MOTION_SAMPLES.fetch_add(1, Ordering::Relaxed) % 100 == 0 {
        diag::log(&format!(
            "sample: cursor=({:.0},{:.0}) delta=({:.0},{:.0}) bbox={:?} remote={} controls_remote={}",
            x,
            y,
            d.0,
            d.1,
            bbox,
            l.screens.iter().filter(|s| !s.is_local).count(),
            c.remote.is_some()
        ));
    }
    let Some((side, name)) = edge_remote(l.screens.iter().filter(|s| !s.is_local), bbox, x, y)
    else {
        // Not pinned: let the user roam the local displays natively; decay stale pushes.
        if let Some(t) = c.last_pin {
            if t.elapsed().as_millis() >= PIN_WINDOW_MS {
                c.pins = 0;
            }
        }
        return;
    };
    // Just-returned grace window: the cursor is parked a few pixels inside the shared edge,
    // so ignore any push into that edge until the user has had a beat to move away from it.
    if let Some(t) = c.cooldown_until {
        if std::time::Instant::now() < t {
            return;
        }
    }
    // Gliding ALONG the edge (e.g. moving down a list that hugs the border) is not a push:
    // the along-axis delta is large while the crossing axis stays clamped.
    let along = match side {
        Side::Right | Side::Left => d.1,
        Side::Top | Side::Bottom => d.0,
    };
    if along.abs() > 3.0 {
        c.pins = 0;
        c.last_pin = None;
        return;
    }
    let now = std::time::Instant::now();
    c.pins = match c.last_pin {
        Some(t) if now.duration_since(t).as_millis() < PIN_WINDOW_MS => c.pins + 1,
        _ => 1,
    };
    c.last_pin = Some(now);
    // Bounce the cursor back inside so the next outward push produces fresh motion events
    // (while pinned at a display edge the OS reports no movement at all).
    let back = bounce_point(side, bbox, x, y);
    input::warp_cursor(back.0, back.1);
    c.last_real = back;
    diag::log(&format!(
        "event-path pin: side={:?} at ({:.0},{:.0}) push={}/{} target={} bbox={:?}",
        side, x, y, c.pins, PIN_THRESHOLD, name, bbox
    ));
    if c.pins >= PIN_THRESHOLD {
        hand_off(c, &l, net, side, &name, back.0, back.1, bbox);
    }
}

/// A secondary has control: forward the motion delta as an absolute position inside its
/// screen, keep the real cursor parked near the anchor (so nothing visibly moves here), and
/// return control when the user crosses back over the shared edge.
fn remote_move(
    c: &mut Ctrl,
    l: std::sync::MutexGuard<'_, Layout>,
    net: &Arc<Mutex<Net>>,
    bbox: (f64, f64, f64, f64),
    primary_name: &str,
    r: RemoteCtrl,
    x: f64,
    y: f64,
    d: (f64, f64),
) {
    let Some(s) = l.screens.iter().find(|s| s.name == r.name) else {
        // The secondary vanished from the layout — take control back.
        c.remote = None;
        crate::sys_cursor::show();
        return;
    };
    let (w, h) = (s.w as f64, s.h as f64);
    drop(l);
    // Throttled diagnostic sample while a secondary has control: shows whether the user's
    // deltas are actually driving the secondary's virtual cursor (and how far it travels),
    // which is the missing half of the picture in any "crossing doesn't work" report.
    if REMOTE_SAMPLES.fetch_add(1, Ordering::Relaxed) % 100 == 0 {
        diag::log(&format!(
            "remote sample: {} vcursor=({:.0},{:.0}) screen={:.0}x{:.0} delta=({:.0},{:.0}) real=({:.0},{:.0})",
            r.name, r.vx, r.vy, w, h, d.0, d.1, x, y
        ));
    }
    let mut vx = r.vx + d.0;
    let mut vy = r.vy + d.1;
    let back = match r.side {
        Side::Right => vx < -1.0,
        Side::Left => vx > w,
        Side::Bottom => vy < -1.0,
        Side::Top => vy > h,
    };
    if back {
        // Crossed back over the shared edge: control returns to the primary. Place the real
        // cursor just inside the local bbox where the virtual cursor exited.
        let (bl, bt, br, bb) = bbox;
        let fy = (vy / h).clamp(0.0, 1.0);
        let fx = (vx / w).clamp(0.0, 1.0);
        let target = match r.side {
            Side::Right => (br - BOUNCE_IN, bt + fy * (bb - bt)),
            Side::Left => (bl + BOUNCE_IN, bt + fy * (bb - bt)),
            Side::Bottom => (bl + fx * (br - bl), bb - BOUNCE_IN),
            Side::Top => (bl + fx * (br - bl), bt + BOUNCE_IN),
        };
        c.remote = None;
        c.pins = 0;
        c.cooldown_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(RETURN_COOLDOWN_MS));
        crate::sys_cursor::show();
        input::warp_cursor(target.0, target.1);
        c.last_real = target;
        info!("control returned to {}", primary_name);
        diag::log(&format!(
            "RETURN <- {} exit=({:.0},{:.0}) target=({:.0},{:.0}) bbox={:?}",
            r.name, vx, vy, target.0, target.1, bbox
        ));
        return;
    }
    vx = vx.clamp(0.0, w - 1.0);
    vy = vy.clamp(0.0, h - 1.0);
    net.lock().unwrap().send_input(&r.name, InputEvent::MouseMove { x: vx, y: vy });
    c.remote = Some(RemoteCtrl { name: r.name.clone(), side: r.side, vx, vy });
    // Re-centre the parked cursor when it drifts, so it never visibly roams the primary.
    let anchor = park_anchor(bbox);
    if (x - anchor.0).abs() > PARK_SLACK || (y - anchor.1).abs() > PARK_SLACK {
        input::warp_cursor(anchor.0, anchor.1);
        c.last_real = anchor;
    }
}

/// Hand control to the secondary `name` attached on `side`. Seeds its virtual cursor at the
/// shared edge (proportionally aligned with where the real cursor left the primary) and parks
/// the real cursor at the centre of the local bbox.
fn hand_off(
    c: &mut Ctrl,
    l: &Layout,
    net: &Arc<Mutex<Net>>,
    side: Side,
    name: &str,
    from_x: f64,
    from_y: f64,
    bbox: (f64, f64, f64, f64),
) {
    let Some(s) = l.screens.iter().find(|s| s.name == name) else { return };
    let (w, h) = (s.w as f64, s.h as f64);
    let (bl, bt, br, bb) = bbox;
    let fx = ((from_x - bl) / (br - bl)).clamp(0.0, 1.0);
    let fy = ((from_y - bt) / (bb - bt)).clamp(0.0, 1.0);
    let (vx, vy) = match side {
        Side::Right => (6.0, 2.0 + fy * (h - 4.0)),
        Side::Left => (w - 7.0, 2.0 + fy * (h - 4.0)),
        Side::Bottom => (2.0 + fx * (w - 4.0), 6.0),
        Side::Top => (2.0 + fx * (w - 4.0), h - 7.0),
    };
    c.remote = Some(RemoteCtrl { name: name.to_string(), side, vx, vy });
    c.pins = 0;
    c.last_pin = None;
    // Hide the local cursor: control now lives on the secondary, and a cursor parked
    // visibly in the middle of the screen reads as "crossing failed".
    crate::sys_cursor::hide();
    net.lock().unwrap().send_input(name, InputEvent::MouseMove { x: vx, y: vy });
    let anchor = park_anchor(bbox);
    input::warp_cursor(anchor.0, anchor.1);
    c.last_real = anchor;
    info!("control handed to {} ({:?})", name, side);
    diag::log(&format!(
        "HAND-OFF -> {} side={:?} entry=({:.0},{:.0}) park=({:.0},{:.0}) bbox={:?}",
        name, side, vx, vy, anchor.0, anchor.1, bbox
    ));
}

/// Is the cursor pinned against an outer edge of the local bbox that has a secondary attached
/// just beyond it? Returns that side and the secondary's name.
fn edge_remote<'a>(
    remotes: impl Iterator<Item = &'a crate::layout::Screen>,
    bbox: (f64, f64, f64, f64),
    x: f64,
    y: f64,
) -> Option<(Side, String)> {
    let (bl, bt, br, bb) = bbox;
    for s in remotes {
        let sl = s.ox as f64;
        let st = s.oy as f64;
        let sr = sl + s.w as f64;
        let sb = st + s.h as f64;
        let overlap_v = st < bb && sb > bt; // overlaps the bbox's vertical span
        let overlap_h = sl < br && sr > bl; // overlaps the bbox's horizontal span
        if x >= br - EDGE_PIN && sl >= br - EDGE_ATTACH && overlap_v {
            return Some((Side::Right, s.name.clone()));
        }
        if x <= bl + EDGE_PIN && sr <= bl + EDGE_ATTACH && overlap_v {
            return Some((Side::Left, s.name.clone()));
        }
        if y >= bb - EDGE_PIN && st >= bb - EDGE_ATTACH && overlap_h {
            return Some((Side::Bottom, s.name.clone()));
        }
        if y <= bt + EDGE_PIN && sb <= bt + EDGE_ATTACH && overlap_h {
            return Some((Side::Top, s.name.clone()));
        }
    }
    None
}

/// After a pin: pull the cursor this far back inside the bbox so the next outward push
/// produces fresh motion events.
fn bounce_point(side: Side, bbox: (f64, f64, f64, f64), x: f64, y: f64) -> (f64, f64) {
    let (bl, bt, br, bb) = bbox;
    match side {
        Side::Right => (br - BOUNCE_IN, y.clamp(bt, bb - 1.0)),
        Side::Left => (bl + BOUNCE_IN, y.clamp(bt, bb - 1.0)),
        Side::Bottom => (x.clamp(bl, br - 1.0), bb - BOUNCE_IN),
        Side::Top => (x.clamp(bl, br - 1.0), bt + BOUNCE_IN),
    }
}

/// Where the real (primary) cursor is parked while a secondary has control: the centre of the
/// local bbox. From there it cannot accidentally touch a shared edge, and every real motion is
/// translated into remote deltas instead of moving anything on this machine.
fn park_anchor(bbox: (f64, f64, f64, f64)) -> (f64, f64) {
    ((bbox.0 + bbox.2) / 2.0, (bbox.1 + bbox.3) / 2.0)
}

/// Forward an input event to the secondary that currently has control — only while a
/// hand-off is active. Local input is never forwarded.
fn forward_if_remote(net: &Arc<Mutex<Net>>, ctrl: &Arc<Mutex<Ctrl>>, ev: InputEvent) {
    let c = ctrl.lock().unwrap();
    if let Some(r) = &c.remote {
        net.lock().unwrap().send_input(&r.name, ev);
    }
}

/// Which outer edge of the local bbox is this remote attached just beyond?
fn attached_side(s: &crate::layout::Screen, bbox: (f64, f64, f64, f64)) -> Option<Side> {
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

/// Rotate control: local machine → each secondary → back to local. Invoked by the hotkey
/// (ScrollLock) on the primary, or relayed from a secondary.
fn cycle_control(
    net: &Arc<Mutex<Net>>,
    layout: &Arc<Mutex<Layout>>,
    ctrl: &Arc<Mutex<Ctrl>>,
    primary_name: &str,
) {
    // Lock order: ctrl, then layout (same as handle_capture).
    let mut c = ctrl.lock().unwrap();
    let l = layout.lock().unwrap();
    if l.screens.len() <= 1 {
        return;
    }
    let Some(bbox) = l.local_bbox() else { return };
    // Unique remote screens in layout order.
    let mut remotes: Vec<&crate::layout::Screen> = Vec::new();
    for s in &l.screens {
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
        // Wrap around: control returns to the primary. The real cursor is already parked at
        // the anchor on this machine — just resume local control.
        c.remote = None;
        c.pins = 0;
        c.cooldown_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(RETURN_COOLDOWN_MS));
        crate::sys_cursor::show();
        info!("control returned to {} (hotkey)", primary_name);
        return;
    }
    let name = remotes[idx].name.clone();
    let side = attached_side(remotes[idx], bbox).unwrap_or(Side::Right);
    let anchor = park_anchor(bbox);
    hand_off(&mut c, &l, net, side, &name, anchor.0, anchor.1, bbox);
}

/// `--probe`: coordinate-space self-test for the crossing pipeline.
///
/// Warps the cursor to a series of known points (screen corners and the shared-edge zone of
/// whatever layout this machine reports), then prints — side by side — the position the
/// event stream reports (`rdev::listen`) and a direct OS read (`CGEventGetLocation`). If the
/// two disagree, or land far from the requested point, the coordinates the crossing logic
/// relies on are broken and the mismatch is right there on the screen.
fn probe() -> anyhow::Result<()> {
    println!("MouseShare coordinate probe");
    let layout = detect_primary_layout("probe-primary");
    let bbox = layout.local_bbox().unwrap_or((0.0, 0.0, 1920.0, 1080.0));
    for s in &layout.screens {
        println!(
            "  screen {} {}x{}@({},{} ) local={}",
            s.name, s.w, s.h, s.ox, s.oy, s.is_local
        );
    }
    println!("  local bbox = {:?}", bbox);

    // Latest position reported by the event stream.
    let last: Arc<Mutex<Option<(f64, f64)>>> = Arc::new(Mutex::new(None));
    {
        let last = last.clone();
        input::start_capture(move |e: Event| {
            if let EventType::MouseMove { x, y } = e.event_type {
                *last.lock().unwrap() = Some((x, y));
            }
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(300)); // let the tap come up

    let (bl, bt, br, bb) = bbox;
    let points: Vec<(&str, f64, f64)> = vec![
        ("local-bbox top-left", bl + 5.0, bt + 5.0),
        ("local-bbox centre", (bl + br) / 2.0, (bt + bb) / 2.0),
        ("local-bbox right edge", br - 2.0, (bt + bb) / 2.0),
        ("local-bbox bottom edge", (bl + br) / 2.0, bb - 2.0),
        ("origin", 1.0, 1.0),
    ];
    for (label, wx, wy) in points {
        // Clear the last-seen marker so the next event must be fresh.
        *last.lock().unwrap() = None;
        input::warp_cursor(wx, wy);
        std::thread::sleep(std::time::Duration::from_millis(350));
        let heard = *last.lock().unwrap();
        let direct = input::cursor_position();
        println!("warp to {:?} ({:.0},{:.0})", label, wx, wy);
        match heard {
            Some((x, y)) => println!("    listen : {:.1},{:.1}", x, y),
            None => println!("    listen : <no event>"),
        }
        match direct {
            Some((x, y)) => println!("    direct : {:.1},{:.1}", x, y),
            None => println!("    direct : <unavailable>"),
        }
    }
    println!("probe done");
    Ok(())
}

/// Build the primary's initial layout from the machine's real displays.
/// On macOS this enumerates every attached screen via `display-info` (Core Graphics), placing each
/// at its true virtual-desktop position — so a Mac with two monitors shows both and the cursor can
/// roam between them natively (each is `is_local = true`). Coordinates come from `CGDisplayBounds`,
/// whose global display space (origin top-left of the main display, y down) matches the cursor
/// coordinates `rdev` reports on macOS, so the layout lines up with reality. On other platforms, or
/// if enumeration fails, we fall back to a single 1080p screen at the origin. Remote (secondary)
/// screens are added later as peers connect (see `Layout::ensure_screen`).
fn detect_primary_layout(primary_name: &str) -> Layout {
    #[cfg(target_os = "macos")]
    {
        match display_info::DisplayInfo::all() {
            Ok(displays) if !displays.is_empty() => {
                // Stable left-to-right, then top-to-bottom order. The main display keeps the bare
                // `primary_name`; the rest get a "#n" suffix (every local screen needs a unique
                // name, but all share `is_local = true` so none of them is ever forwarded).
                let mut d: Vec<_> = displays.into_iter().collect();
                d.sort_by(|a, b| a.x.cmp(&b.x).then_with(|| a.y.cmp(&b.y)));
                let mut screens = Vec::with_capacity(d.len());
                for (i, disp) in d.iter().enumerate() {
                    let name = if disp.is_primary || i == 0 {
                        primary_name.to_string()
                    } else {
                        format!("{} #{}", primary_name, i + 1)
                    };
                    screens.push(crate::layout::Screen {
                        name,
                        ox: disp.x,
                        oy: disp.y,
                        w: disp.width,
                        h: disp.height,
                        is_local: true,
                    });
                }
                info!(
                    "detected {} display(s) on primary: {}",
                    screens.len(),
                    screens
                        .iter()
                        .map(|s| format!("{} {}x{}@({},{}))", s.name, s.w, s.h, s.ox, s.oy))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return Layout { screens };
            }
            Ok(_) => log::warn!("no displays reported; falling back to a single 1080p screen"),
            Err(e) => log::warn!("display enumeration failed ({}); falling back to single screen", e),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = primary_name;
    }
    Layout {
        screens: vec![crate::layout::Screen {
            name: primary_name.to_string(),
            ox: 0,
            oy: 0,
            w: 1920,
            h: 1080,
            is_local: true,
        }],
    }
}
