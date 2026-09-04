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
#![cfg_attr(target_os = "windows", windows_subsystem = "Windows")]

mod app;
mod clipboard;
mod config;
mod i18n;
mod input;
mod layout;
mod network;
mod protocol;

use crate::config::{load_config, save_config, Config};
use crate::i18n::Lang;
use crate::layout::{Layout, Screen};
use crate::network::{connect_client, start_hub, Net};
use crate::protocol::{InputEvent, Message};
use log::info;
use rdev::{display_size, Event, EventType, Key};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

fn main() -> anyhow::Result<()> {
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

    // Shared state used by the capture thread.
    let ownership: Arc<Mutex<String>> = Arc::new(Mutex::new(primary_name.clone()));
    let vcursor: Arc<Mutex<(f64, f64)>> = Arc::new(Mutex::new((0.0, 0.0)));
    let last_real: Arc<Mutex<(f64, f64)>> = Arc::new(Mutex::new((-1.0, -1.0))); // sentinel: uninitialised

    // ---- Incoming message handler ----
    {
        let net = net.clone();
        let layout = layout.clone();
        let last_seen = Arc::new(Mutex::new(String::new()));
        let mode2 = mode.clone();
        let ownership = ownership.clone();
        let vcursor = vcursor.clone();
        let last_real = last_real.clone();
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
                            cycle_control(&net, &layout, &ownership, &vcursor, &last_real, &primary_name);
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
        let net = net.clone();
        let layout = layout.clone();
        let ownership = ownership.clone();
        let vcursor = vcursor.clone();
        let last_real = last_real.clone();
        let primary_name = primary_name.clone();
        input::start_capture(move |event: Event| {
            handle_capture(
                event,
                &net,
                &layout,
                &ownership,
                &vcursor,
                &last_real,
                &primary_name,
            );
        });
    } else {
        info!("running as secondary; waiting for input from {}", server_addr);
        // Secondaries also listen for the switch hotkey (ScrollLock) so the user can hand control
        // back to the primary from the Windows side — the press is forwarded to the primary, which
        // actually rotates ownership.
        let net_hk = net.clone();
        input::start_capture(move |e: Event| {
            if let EventType::KeyPress(Key::ScrollLock) = e.event_type {
                net_hk.lock().unwrap().send_message(Message::Hotkey);
            }
        });
    }

    // ---- GUI on the main thread ----
    let gui_app = app::MouseShareApp::new(config, layout, net, my_name, startup_error, inc_tx.clone());

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

#[allow(clippy::too_many_arguments)]
fn handle_capture(
    event: Event,
    net: &Arc<Mutex<Net>>,
    layout: &Arc<Mutex<Layout>>,
    ownership: &Arc<Mutex<String>>,
    vcursor: &Arc<Mutex<(f64, f64)>>,
    last_real: &Arc<Mutex<(f64, f64)>>,
    primary_name: &str,
) {
    match event.event_type {
        EventType::MouseMove { x, y } => {
            // Initialise the "last real cursor" baseline on the first event.
            {
                let mut lr = last_real.lock().unwrap();
                if lr.0 < -0.5 {
                    *lr = (x, y);
                    return;
                }
            }
            let d = {
                let mut lr = last_real.lock().unwrap();
                let d = (x - lr.0, y - lr.1);
                *lr = (x, y);
                d
            };

            let l = layout.lock().unwrap();
            // With a single screen there is nothing to hand control to — the real cursor just
            // moves locally and nothing is forwarded.
            if l.screens.len() <= 1 {
                return;
            }
            let bbox = l.local_bbox().unwrap_or((0.0, 0.0, 1920.0, 1080.0));
            let cur = ownership.lock().unwrap().clone();
            let cur_is_local = l.screens.iter().find(|s| s.name == cur).map(|s| s.is_local).unwrap_or(true);

            if cur_is_local {
                // The real cursor is free on this primary. If it is over one of our own displays,
                // just keep tracking it locally — never forward to a secondary (this is what lets a
                // multi-monitor Mac roam between its own screens natively).
                if let Some(idx) = l.screen_at(x, y).filter(|&i| l.screens[i].is_local) {
                    *ownership.lock().unwrap() = l.screens[idx].name.clone();
                    *vcursor.lock().unwrap() = (x, y);
                    return;
                }
                // It's at/over an edge or a gap: try to hand control off to an adjacent secondary.
                if let Some((side, rname)) = outward_handoff(&l, bbox, x, y, d) {
                    let remote = match l.screens.iter().find(|s| s.name == rname) {
                        Some(s) => s.clone(),
                        None => return,
                    };
                    *ownership.lock().unwrap() = rname.clone();
                    // Seed the secondary cursor at the shared edge, then apply this first delta.
                    let mut v = vcursor.lock().unwrap();
                    *v = entry_point(side, &remote, x, y);
                    *v = (v.0 + d.0, v.1 + d.1);
                    *v = clamp_to_remote(&remote, *v);
                    let (fx, fy) = *v;
                    drop(v);
                    net.lock().unwrap().send_input(&rname, InputEvent::MouseMove { x: fx, y: fy });
                    // Park the real cursor on the opposite edge of the local bbox so it keeps
                    // producing outward motion; without this the OS would clamp it at the display
                    // edge and the secondary cursor would stall.
                    let park = park_point(side, bbox, x, y);
                    drop(l);
                    input::warp_cursor(park.0, park.1);
                    *last_real.lock().unwrap() = park;
                }
                // else: over a gap with no secondary beyond it — do nothing.
            } else {
                // Controlling a secondary: forward deltas, keep the real cursor parked, and watch
                // for the user pushing back inward (which returns control to this primary). The
                // virtual cursor here lives in the *secondary's* local space, so it can never drift
                // back onto the primary's screen and cause the two to move at once.
                let remote = match l.screens.iter().find(|s| s.name == cur) {
                    Some(s) => s.clone(),
                    None => {
                        *ownership.lock().unwrap() = primary_name.to_string();
                        return;
                    }
                };
                let side = side_of_remote(bbox, &remote);
                if is_outward(side, d) {
                    let mut v = vcursor.lock().unwrap();
                    *v = (v.0 + d.0, v.1 + d.1);
                    *v = clamp_to_remote(&remote, *v);
                    let (fx, fy) = *v;
                    drop(v);
                    net.lock().unwrap().send_input(&cur, InputEvent::MouseMove { x: fx, y: fy });
                    let park = park_point(side, bbox, x, y);
                    drop(l);
                    input::warp_cursor(park.0, park.1);
                    *last_real.lock().unwrap() = park;
                } else if is_inward(side, d) {
                    // User moved back toward the primary — release control. The real cursor is
                    // already on this machine, so we simply stop parking/forwarding.
                    *ownership.lock().unwrap() = primary_name.to_string();
                    *vcursor.lock().unwrap() = (x, y);
                }
                // else: delta ~0 (real cursor clamped at the edge) — stay parked, do nothing.
            }
        }

        EventType::ButtonPress(b) => {
            let ev = InputEvent::MouseDown {
                button: input::button_to_ms(b),
            };
            forward_if_remote(net, ownership, layout, ev);
        }
        EventType::ButtonRelease(b) => {
            let ev = InputEvent::MouseUp {
                button: input::button_to_ms(b),
            };
            forward_if_remote(net, ownership, layout, ev);
        }

        EventType::Wheel { delta_x, delta_y } => {
            forward_if_remote(
                net,
                ownership,
                layout,
                InputEvent::Wheel { dx: delta_x, dy: delta_y },
            );
        }

        EventType::KeyPress(k) => {
            // The hotkey (default ScrollLock) rotates control to the next machine.
            if k == Key::ScrollLock {
                cycle_control(net, layout, ownership, vcursor, last_real, primary_name);
                return;
            }
            forward_if_remote(net, ownership, layout, InputEvent::KeyDown { key: k });
        }
        EventType::KeyRelease(k) => {
            // Ignore the hotkey's own release so a single tap cycles exactly once.
            if k == Key::ScrollLock {
                return;
            }
            forward_if_remote(net, ownership, layout, InputEvent::KeyUp { key: k });
        }
    }
}

/// Forward an input event to the screen that currently has control, but only when that screen is
/// a *remote* (secondary) one. When control is on a local screen, nothing is forwarded — the real
/// cursor/keyboard is already acting on this machine.
fn forward_if_remote(
    net: &Arc<Mutex<Net>>,
    ownership: &Arc<Mutex<String>>,
    layout: &Arc<Mutex<Layout>>,
    ev: InputEvent,
) {
    let l = layout.lock().unwrap();
    let cur = ownership.lock().unwrap().clone();
    let is_remote = l.screens.iter().find(|s| s.name == cur).map(|s| !s.is_local).unwrap_or(false);
    drop(l);
    if is_remote {
        net.lock().unwrap().send_input(&cur, ev);
    }
}

/// The side of the local bounding box a remote screen is attached to (by the smallest gap).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Right,
    Left,
    Top,
    Bottom,
}

/// Clamp a point to the local coordinate space of a remote screen (origin at its top-left).
fn clamp_to_remote(s: &Screen, p: (f64, f64)) -> (f64, f64) {
    (p.0.clamp(0.0, s.w as f64 - 1.0), p.1.clamp(0.0, s.h as f64 - 1.0))
}

/// Which side of the local bounding box a remote screen is attached to.
fn side_of_remote(bbox: (f64, f64, f64, f64), s: &Screen) -> Side {
    let (l, t, r, b) = bbox;
    let ro = s.ox as f64;
    let rr = (s.ox + s.w as i32) as f64;
    let rt = s.oy as f64;
    let rb = (s.oy + s.h as i32) as f64;
    let g_right = (ro - r).abs();
    let g_left = (l - rr).abs();
    let g_bottom = (rt - b).abs();
    let g_top = (t - rb).abs();
    let mut best = Side::Right;
    let mut bestv = g_right;
    if g_left < bestv {
        bestv = g_left;
        best = Side::Left;
    }
    if g_bottom < bestv {
        bestv = g_bottom;
        best = Side::Bottom;
    }
    if g_top < bestv {
        best = Side::Top;
    }
    best
}

/// Where the cursor appears on a remote screen (its local coords) when control is handed off —
/// the shared edge, aligned with the real cursor on the crossing axis.
fn entry_point(side: Side, s: &Screen, real_x: f64, real_y: f64) -> (f64, f64) {
    let (ox, oy, w, h) = (s.ox as f64, s.oy as f64, s.w as f64, s.h as f64);
    match side {
        Side::Right => (0.0, (real_y - oy).clamp(0.0, h - 1.0)),
        Side::Left => (w - 1.0, (real_y - oy).clamp(0.0, h - 1.0)),
        Side::Top => ((real_x - ox).clamp(0.0, w - 1.0), 0.0),
        Side::Bottom => ((real_x - ox).clamp(0.0, w - 1.0), h - 1.0),
    }
}

/// Where to park the real (primary) cursor so it keeps generating outward motion while we drive
/// the secondary: the edge of the local bbox *opposite* the shared edge.
fn park_point(side: Side, bbox: (f64, f64, f64, f64), real_x: f64, real_y: f64) -> (f64, f64) {
    let (l, t, r, b) = bbox;
    let m = 4.0;
    match side {
        Side::Right => (l + m, real_y.clamp(t, b)),
        Side::Left => (r - m, real_y.clamp(t, b)),
        Side::Top => (real_x.clamp(l, r), b - m),
        Side::Bottom => (real_x.clamp(l, r), t + m),
    }
}

fn is_outward(side: Side, d: (f64, f64)) -> bool {
    match side {
        Side::Right => d.0 > 0.0,
        Side::Left => d.0 < 0.0,
        Side::Top => d.1 < 0.0,
        Side::Bottom => d.1 > 0.0,
    }
}

fn is_inward(side: Side, d: (f64, f64)) -> bool {
    match side {
        Side::Right => d.0 < 0.0,
        Side::Left => d.0 > 0.0,
        Side::Top => d.1 > 0.0,
        Side::Bottom => d.1 < 0.0,
    }
}

/// Find a secondary attached to the local bbox on the side the cursor is pushing toward (near the
/// edge, moving outward, and overlapping on the crossing axis). Used for automatic edge hand-off.
fn outward_handoff(
    l: &Layout,
    bbox: (f64, f64, f64, f64),
    x: f64,
    y: f64,
    d: (f64, f64),
) -> Option<(Side, String)> {
    let (bl, bt, br, bb) = bbox;
    let m = 6.0;
    let near_right = x >= br - m;
    let near_left = x <= bl + m;
    let near_bottom = y >= bb - m;
    let near_top = y <= bt + m;
    for s in &l.screens {
        if s.is_local {
            continue;
        }
        let side = side_of_remote(bbox, s);
        let adjacent = match side {
            Side::Right => (s.ox as f64 - br).abs() < 160.0,
            Side::Left => (bl - (s.ox + s.w as i32) as f64).abs() < 160.0,
            Side::Bottom => (s.oy as f64 - bb).abs() < 160.0,
            Side::Top => (bt - (s.oy + s.h as i32) as f64).abs() < 160.0,
        };
        if !adjacent {
            continue;
        }
        let ok = match side {
            Side::Right => near_right && is_outward(Side::Right, d) && overlap_y(s, bt, bb),
            Side::Left => near_left && is_outward(Side::Left, d) && overlap_y(s, bt, bb),
            Side::Bottom => near_bottom && is_outward(Side::Bottom, d) && overlap_x(s, bl, br),
            Side::Top => near_top && is_outward(Side::Top, d) && overlap_x(s, bl, br),
        };
        if ok {
            return Some((side, s.name.clone()));
        }
    }
    None
}

/// Rotate control to the next machine: the primary (as one machine) then each secondary, cycling.
/// Called when the hotkey is pressed locally or relayed from a secondary.
fn cycle_control(
    net: &Arc<Mutex<Net>>,
    layout: &Arc<Mutex<Layout>>,
    ownership: &Arc<Mutex<String>>,
    vcursor: &Arc<Mutex<(f64, f64)>>,
    last_real: &Arc<Mutex<(f64, f64)>>,
    primary_name: &str,
) {
    let l = layout.lock().unwrap();
    let mut remotes: Vec<String> = Vec::new();
    for s in &l.screens {
        if !s.is_local && !remotes.iter().any(|n| n == &s.name) {
            remotes.push(s.name.clone());
        }
    }
    if remotes.is_empty() {
        return; // nothing else to switch to
    }
    let cur = ownership.lock().unwrap().clone();
    let cur_is_local = l.screens.iter().find(|s| s.name == cur).map(|s| s.is_local).unwrap_or(true);
    let cur_machine: String = if cur_is_local {
        "__local__".to_string()
    } else {
        cur.clone()
    };
    let mut order = vec!["__local__".to_string()];
    order.extend(remotes);
    let idx = order.iter().position(|m| m == &cur_machine).unwrap_or(0);
    let next = order[(idx + 1) % order.len()].clone();
    drop(l);

    if next == "__local__" {
        *ownership.lock().unwrap() = primary_name.to_string();
        *vcursor.lock().unwrap() = *last_real.lock().unwrap();
    } else {
        let l2 = layout.lock().unwrap();
        let remote = match l2.screens.iter().find(|s| s.name == next) {
            Some(s) => s.clone(),
            None => return,
        };
        let bbox = l2.local_bbox().unwrap_or((0.0, 0.0, 1920.0, 1080.0));
        let side = side_of_remote(bbox, &remote);
        let lr = *last_real.lock().unwrap();
        *ownership.lock().unwrap() = next.clone();
        let mut v = vcursor.lock().unwrap();
        *v = entry_point(side, &remote, lr.0, lr.1);
        drop(v);
        // Park the real cursor on the opposite edge and seed the secondary cursor at the entry.
        let park = park_point(side, bbox, lr.0, lr.1);
        drop(l2);
        input::warp_cursor(park.0, park.1);
        *last_real.lock().unwrap() = park;
        let v = *vcursor.lock().unwrap();
        net.lock().unwrap().send_input(&next, InputEvent::MouseMove { x: v.0, y: v.1 });
    }
}

/// Do two screen vertical ranges overlap? Used to decide whether a remote screen sits just
/// beyond the local bbox's left/right edge (so the hand-off may engage on that edge).
fn overlap_y(s: &Screen, top: f64, bottom: f64) -> bool {
    let st = s.oy as f64;
    let sb = s.oy as f64 + s.h as f64;
    st < bottom && sb > top
}

/// Do two screen horizontal ranges overlap? Used for the top/bottom edges.
fn overlap_x(s: &Screen, left: f64, right: f64) -> bool {
    let sl = s.ox as f64;
    let sr = s.ox as f64 + s.w as f64;
    sl < right && sr > left
}

/// Build the primary's initial layout from the machine's real displays.
///
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
