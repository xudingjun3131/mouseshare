//! MouseShare — share mouse / keyboard / clipboard across computers over LAN.
//!
//! Architecture
//! -----------
//! * The **primary** runs a TCP hub and captures the real input via `rdev::listen`.
//! * **Secondaries** connect to the primary and inject the forwarded input via `rdev::simulate`.
//! * A shared `Layout` (edited in the GUI) defines where each machine's screen sits in a virtual
//!   desktop; crossing an edge hands control (and the cursor) to the neighbour.
//! * Clipboard changes are broadcast and loop-suppressed.

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
use crate::layout::Layout;
use crate::network::{connect_client, start_hub, Net};
use crate::protocol::{InputEvent, Message};
use log::info;
use rdev::{display_size, Button as RdevButton, Event, EventType};
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

    // ---- Capture (primary only) ----
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

            // Advance the unbounded virtual cursor.
            {
                let mut vc = vcursor.lock().unwrap();
                *vc = (vc.0 + d.0, vc.1 + d.1);
            }

            let (cx, cy, owning_idx) = {
                let l = layout.lock().unwrap();
                let vc = *vcursor.lock().unwrap();
                let owning_idx = l.screen_at(vc.0, vc.1);
                let (cx, cy) = if owning_idx.is_some() {
                    (vc.0, vc.1)
                } else {
                    l.clamp(vc.0, vc.1)
                };
                (cx, cy, owning_idx)
            };

            if let Some(idx) = owning_idx {
                let (name, ox, oy, is_local) = {
                    let l = layout.lock().unwrap();
                    let s = &l.screens[idx];
                    (s.name.clone(), s.ox as f64, s.oy as f64, s.is_local)
                };
                *ownership.lock().unwrap() = name.clone();
                if !is_local {
                    // Remote screen: inject the absolute position *inside* that screen.
                    net.lock()
                        .unwrap()
                        .send_input(&name, InputEvent::MouseMove { x: cx - ox, y: cy - oy });
                }
                // Local screen: the real cursor is already there — nothing to inject. This is what
                // lets the primary's own multiple monitors work: every one is `is_local`, so the
                // cursor roams between them natively without any forwarding.
            } else {
                // In a gap: keep forwarding to the current owner (clamped). Only matters when a
                // screen was dragged away leaving a dead band — normally the secondary sits flush
                // against the local bbox, so the cursor never lands here.
                let cur = ownership.lock().unwrap().clone();
                let l = layout.lock().unwrap();
                if let Some(s) = l.screens.iter().find(|s| s.name == cur) {
                    if !s.is_local {
                        let (gx, gy) = l.clamp(cx, cy);
                        net.lock().unwrap().send_input(
                            &cur,
                            InputEvent::MouseMove {
                                x: gx - s.ox as f64,
                                y: gy - s.oy as f64,
                            },
                        );
                    }
                }
                let mut vc = vcursor.lock().unwrap();
                *vc = l.clamp(cx, cy);
            }

            // Treadmill: keep the physical cursor producing motion once it hits an edge of the
            // primary's own (local) bounding box while still pushing outward, so the virtual cursor
            // can keep marching onto the neighbouring remote screen. We only warp on an edge that
            // actually has a remote screen just beyond it, and we warp the real cursor to the
            // *opposite* edge of the local bbox so it stays on the primary. Without this, the OS
            // would clamp the real cursor at the display edge and the virtual cursor would stall.
            let l = layout.lock().unwrap();
            if l.screens.len() > 1 {
                // Is the real cursor currently on one of the primary's own (local) screens?
                if let Some(local_idx) = l.screen_at(x, y).filter(|&i| l.screens[i].is_local) {
                    let ls = &l.screens[local_idx];
                    let bbox = l.local_bbox().unwrap_or((
                        ls.ox as f64,
                        ls.oy as f64,
                        ls.ox as f64 + ls.w as f64,
                        ls.oy as f64 + ls.h as f64,
                    ));
                    let (bb_left, bb_top, bb_right, bb_bottom) = bbox;
                    let margin = 4.0;
                    let near_right = x >= bb_right - margin;
                    let near_left = x <= bb_left + margin;
                    let near_bottom = y >= bb_bottom - margin;
                    let near_top = y <= bb_top + margin;
                    // Is there a remote screen just beyond each edge (overlapping on the crossing axis)?
                    let remote_right = l.screens.iter().any(|s| {
                        !s.is_local
                            && (s.ox as f64) >= bb_right - 2.0
                            && (s.ox as f64) <= bb_right + 120.0
                            && overlap_y(s, bb_top, bb_bottom)
                    });
                    let remote_left = l.screens.iter().any(|s| {
                        !s.is_local
                            && ((s.ox + s.w as i32) as f64) <= bb_left + 2.0
                            && ((s.ox + s.w as i32) as f64) >= bb_left - 120.0
                            && overlap_y(s, bb_top, bb_bottom)
                    });
                    let remote_bottom = l.screens.iter().any(|s| {
                        !s.is_local
                            && (s.oy as f64) >= bb_bottom - 2.0
                            && (s.oy as f64) <= bb_bottom + 120.0
                            && overlap_x(s, bb_left, bb_right)
                    });
                    let remote_top = l.screens.iter().any(|s| {
                        !s.is_local
                            && ((s.oy + s.h as i32) as f64) <= bb_top + 2.0
                            && ((s.oy + s.h as i32) as f64) >= bb_top - 120.0
                            && overlap_x(s, bb_left, bb_right)
                    });
                    let pushing_out = (remote_right && near_right && d.0 > 0.0)
                        || (remote_left && near_left && d.0 < 0.0)
                        || (remote_bottom && near_bottom && d.1 > 0.0)
                        || (remote_top && near_top && d.1 < 0.0);
                    if pushing_out {
                        let nx = if remote_right {
                            bb_left + margin
                        } else if remote_left {
                            bb_right - margin
                        } else {
                            x
                        };
                        let ny = if remote_bottom {
                            bb_top + margin
                        } else if remote_top {
                            bb_bottom - margin
                        } else {
                            y
                        };
                        input::warp_cursor(nx, ny);
                        *last_real.lock().unwrap() = (nx, ny);
                    }
                }
            }
        }

        EventType::ButtonPress(b) => forward_button(net, ownership, primary_name, b, true),
        EventType::ButtonRelease(b) => forward_button(net, ownership, primary_name, b, false),

        EventType::Wheel { delta_x, delta_y } => {
            let target = ownership.lock().unwrap().clone();
            if target != primary_name {
                net.lock()
                    .unwrap()
                    .send_input(&target, InputEvent::Wheel { dx: delta_x, dy: delta_y });
            }
        }

        EventType::KeyPress(k) => {
            let target = ownership.lock().unwrap().clone();
            if target != primary_name {
                net.lock().unwrap().send_input(&target, InputEvent::KeyDown { key: k });
            }
        }
        EventType::KeyRelease(k) => {
            let target = ownership.lock().unwrap().clone();
            if target != primary_name {
                net.lock().unwrap().send_input(&target, InputEvent::KeyUp { key: k });
            }
        }
    }
}

fn forward_button(
    net: &Arc<Mutex<Net>>,
    ownership: &Arc<Mutex<String>>,
    primary_name: &str,
    b: RdevButton,
    down: bool,
) {
    let target = ownership.lock().unwrap().clone();
    if target != primary_name {
        let ev = if down {
            InputEvent::MouseDown {
                button: input::button_to_ms(b),
            }
        } else {
            InputEvent::MouseUp {
                button: input::button_to_ms(b),
            }
        };
        net.lock().unwrap().send_input(&target, ev);
    }
}

/// Do two screen vertical ranges overlap? Used to decide whether a remote screen sits just
/// beyond the local bbox's left/right edge (so the treadmill may engage on that edge).
fn overlap_y(s: &crate::layout::Screen, top: f64, bottom: f64) -> bool {
    let st = s.oy as f64;
    let sb = s.oy as f64 + s.h as f64;
    st < bottom && sb > top
}

/// Do two screen horizontal ranges overlap? Used for the top/bottom edges.
fn overlap_x(s: &crate::layout::Screen, left: f64, right: f64) -> bool {
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
