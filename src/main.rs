//! MouseShare — share mouse / keyboard / clipboard across computers over LAN.
//!
//! # Architecture (rewrite: native grab + relative deltas)
//!
//! * The **primary** runs a TCP hub and *grabs* the real input via a `CGEventTap` (macOS) — the tap
//!   returns `None` for events while a secondary has control, so the OS never sees them and never
//!   clamps the cursor at a display edge.
//! * The capture layer forwards **relative mouse deltas** (`MouseMotion { dx, dy }`); the receiver
//!   accumulates them against its own real cursor, so no screen-geometry/DPI agreement is needed.
//! * Crossing is predicted from `location + delta` *before* the cursor reaches an edge. There is no
//!   treadmill, no edge-rest poller, no park-and-bounce — those were band-aids for the old observer
//!   architecture (which couldn't keep the delta stream alive once the OS pinned the cursor).
//! * **Secondaries** receive input and inject it relatively; they run no capture of their own (only a
//!   lightweight hotkey listener to hand control back).
//! * Network / clipboard / config / egui UI are unchanged from the previous build.

// On Windows, build a GUI-subsystem executable (no black console window).
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod capture;
mod clipboard;
mod discovery;
mod config;
mod control;
mod diag;
mod i18n;
mod input;
mod layout;
mod network;
mod protocol;
mod single_instance;
#[cfg(target_os = "windows")]
mod tray;

// `app.rs` historically referenced `crate::Ctrl`; keep that path working after the move.
pub use control::Ctrl;

use log::info;

use crate::config::{load_config, save_config, Config};
use crate::control::{CaptureMode, GrabCtx, HotkeyState, on_enter_screen, on_leave_screen, on_secondary_input, cycle_control, return_control};
use crate::i18n::Lang;
use crate::layout::Layout;
use crate::network::{connect_client, start_hub, Net};
use crate::protocol::Message;
use std::sync::mpsc::channel;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

fn main() -> anyhow::Result<()> {
    // Windows: declare per-monitor DPI awareness BEFORE anything queries display metrics, so
    // absolute coordinate spaces match between the event stream and injection.
    #[cfg(target_os = "windows")]
    {
        #[link(name = "user32")]
        extern "system" {
            fn SetProcessDpiAwarenessContext(ctx: isize) -> i32;
            fn SetProcessDPIAware() -> i32;
        }
        const PER_MONITOR_AWARE_V2: isize = -4;
        unsafe {
            if SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) == 0 {
                SetProcessDPIAware();
            }
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Refuse to run twice (two copies would fight over the capture tap + listen port).
    let _instance_guard = match single_instance::acquire() {
        Some(g) => g,
        None => {
            single_instance::notify_already_running();
            log::warn!("another MouseShare instance is already running; exiting");
            return Ok(());
        }
    };

    let mut config: Config = load_config();
    let my_name = config.name.clone();

    #[cfg(target_os = "windows")]
    tray::init(Lang::from_code(&config.lang));

    // Auto-detect the primary's LAN IP so the GUI shows a connectable address.
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

    let (inc_tx, inc_rx) = channel::<(String, Message)>();

    let mut startup_error: Option<String> = None;

    // Shared layout state (hub pushes screens to secondaries, capture reads bounds for crossing).
    let layout: Arc<Mutex<Layout>> = Arc::new(Mutex::new(if mode == "primary" {
        detect_primary_layout(&primary_name)
    } else {
        config.layout.clone()
    }));

    // This host's own display rectangle(s): used to seed/clamp the virtual cursor while a secondary
    // is being driven, and as the capture bbox on the primary.
    let own_layout: Layout = if mode == "primary" {
        layout.lock().unwrap().clone()
    } else {
        detect_primary_layout(&my_name)
    };
    input::set_local_layout(&own_layout);
    // What we advertise to the hub: our local screens' bounding box (logical units) plus the UI
    // scale of that coordinate space, so the primary can normalise forwarded mouse deltas.
    let (my_w, my_h, my_scale) = hello_metrics(&own_layout);

    let net: Arc<Mutex<Net>> = if mode == "primary" {
        match start_hub(port, inc_tx.clone(), layout.clone()) {
            Ok(n) => n,
            Err(e) => {
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
                tx.send(Message::Hello {
                    name: my_name.clone(),
                    width: my_w,
                    height: my_h,
                    scale: my_scale,
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

    // Control plane.
    let ctrl: Arc<Mutex<Ctrl>> = Arc::new(Mutex::new(Ctrl {
        local_bbox: own_layout.local_bbox(),
        ..Default::default()
    }));
    let grab_ctx: Arc<GrabCtx> = Arc::new(GrabCtx {
        net: net.clone(),
        layout: layout.clone(),
        ctrl: ctrl.clone(),
        mode: Mutex::new(CaptureMode::Local),
        my_name: my_name.clone(),
        primary_name: primary_name.clone(),
    });

    // ---- Startup diagnostics dump (file-based; stderr is invisible when launched from Finder) ----
    {
        let l = layout.lock().unwrap();
        let screens = l
            .screens
            .iter()
            .map(|s| {
                let phys = s.physical_size();
                if phys != (s.w, s.h) {
                    format!(
                        "{}({}x{}@{},{} physical={}x{} local={})",
                        s.name, s.w, s.h, s.ox, s.oy, phys.0, phys.1, s.is_local
                    )
                } else {
                    format!("{}({}x{}@{},{} local={})", s.name, s.w, s.h, s.ox, s.oy, s.is_local)
                }
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
        let grab_ctx = grab_ctx.clone();
        let mode2 = mode.clone();
        std::thread::spawn(move || {
            for (from, msg) in inc_rx {
                match msg {
                    Message::Clipboard { text } => {
                        if mode2 == "secondary" {
                            clipboard::set_clipboard(&text);
                        } else {
                            clipboard::set_clipboard(&text);
                            net.lock().unwrap().broadcast_clipboard(&text, Some(&from));
                        }
                    }
                    Message::Input(ev) => {
                        // Only a secondary applies forwarded input (it is being driven).
                        if mode2 == "secondary" {
                            on_secondary_input(&grab_ctx, ev);
                        }
                    }
                    Message::Hello { name, width, height, scale } => {
                        if mode2 == "primary" {
                            if layout
                                .lock()
                                .unwrap()
                                .ensure_screen(&name, width, height, false, scale)
                            {
                                info!("auto-registered screen for peer {}", name);
                            }
                        }
                    }
                    Message::Layout { layout: new_layout } => {
                        if mode2 == "secondary" {
                            *layout.lock().unwrap() = new_layout;
                        }
                    }
                    Message::EnterScreen { side, fx, fy } => {
                        if mode2 == "secondary" {
                            on_enter_screen(&grab_ctx, side, fx, fy);
                        }
                    }
                    Message::LeaveScreen => {
                        if mode2 == "secondary" {
                            on_leave_screen(&grab_ctx);
                        }
                    }
                    Message::Hotkey => {
                        // A secondary pressed the switch hotkey; only the primary rotates control.
                        if mode2 == "primary" {
                            cycle_control(&grab_ctx);
                        }
                    }
                    Message::ReturnControl => {
                        // A secondary's virtual cursor was pushed back across the shared edge;
                        // only the primary hands control back (it is the one forwarding).
                        if mode2 == "primary" {
                            return_control(&grab_ctx);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // ---- LAN discovery ----
    // The primary broadcasts a UDP beacon so secondaries can find it without the user typing an
    // IP; a secondary listens and auto-connects whenever it is not already linked. This is what
    // makes a fresh Windows box "see" the Mac on the network.
    let discovered: discovery::DiscoveredList = discovery::new_list();
    if mode == "primary" {
        discovery::start_beacon(port, my_name.clone());
        info!("discovery beacon broadcasting on udp/{}", discovery::DISCOVERY_PORT);
    } else {
        let net_d = net.clone();
        let inc_tx_d = inc_tx.clone();
        let discovered_d = discovered.clone();
        let my_name_d = my_name.clone();
        let auto_guard = Arc::new(Mutex::new(false));
        discovery::start_listener(move |d: discovery::Discovered| {
            // Record for the UI's "discovered devices" card.
            {
                let mut v = discovered_d.lock().unwrap();
                if !v.iter().any(|x| x.ip == d.ip && x.name == d.name) {
                    v.push(d.clone());
                }
            }
            // Auto-connect only when we're currently unlinked and not mid-retry.
            let is_linked = matches!(*net_d.lock().unwrap(), Net::Secondary { .. });
            let mut g = auto_guard.lock().unwrap();
            if is_linked || *g {
                return;
            }
            *g = true;
            let addr = d.addr();
            drop(g);
            match connect_client(&addr, inc_tx_d.clone(), net_d.clone()) {
                Ok((_net_inner, tx)) => {
                    let _ = tx.send(Message::Hello {
                        name: my_name_d.clone(),
                        width: my_w,
                        height: my_h,
                        scale: my_scale,
                    });
                    info!("auto-connected to primary {}", addr);
                    *auto_guard.lock().unwrap() = false;
                }
                Err(e) => {
                    log::warn!("auto-connect to {} failed: {}", addr, e);
                    *auto_guard.lock().unwrap() = false;
                }
            }
        });
    }

    // ---- Clipboard monitor (both roles) ----
    {
        let net = net.clone();
        clipboard::start_monitor(Arc::new(Mutex::new(String::new())), move |text: String| {
            net.lock().unwrap().broadcast_clipboard(&text, None);
        });
    }

    // ---- Capture ----
    // Shared "permission denied" flag: set by the capture thread when it cannot create the event
    // tap, polled by the GUI so it can pop the native prompt + a guidance dialog with re-check.
    let capture_failed: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    if mode == "primary" {
        // Native grab: CGEventTap on macOS; rdev observer (legacy) elsewhere.
        capture::start_capture(grab_ctx.clone(), capture_failed.clone());
    } else {
        info!("running as secondary; waiting for input from {}", server_addr);
        // Lightweight hotkey listener so the user can hand control back from the Windows/Mac side.
        let net_hk = net.clone();
        let hk = Arc::new(Mutex::new(HotkeyState::default()));
        std::thread::spawn(move || {
            let _ = rdev::listen(move |e: rdev::Event| {
                let (k, down) = match e.event_type {
                    rdev::EventType::KeyPress(k) => (k, true),
                    rdev::EventType::KeyRelease(k) => (k, false),
                    _ => return,
                };
                let mut st = hk.lock().unwrap();
                if hotkey_fired(k, down, &mut st) {
                    drop(st);
                    net_hk.lock().unwrap().send_message(Message::Hotkey);
                }
            });
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
        discovered,
        grab_ctx.clone(),
        capture_failed,
    );

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../resources/mouse-logo.png"))
        .map(Arc::new)
        .ok();

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1240.0, 780.0])
        .with_min_inner_size([940.0, 600.0]);
    // macOS: treat the window as a native "unified toolbar" app — let the content draw edge to
    // edge under the title bar and float the red/yellow/green traffic lights over it. This is
    // what makes MouseShare read as a first-class macOS app instead of a generic GL canvas.
    #[cfg(target_os = "macos")]
    {
        viewport = viewport
            .with_fullsize_content_view(true)
            .with_title_shown(false)
            .with_titlebar_buttons_shown(true);
    }
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

/// Switch hotkey detection (ScrollLock, or Ctrl+Alt+Space). Returns `true` only on the press that
/// fires. Shared by the secondary hotkey listener; the primary detects it inside the grab tap.
fn hotkey_fired(k: rdev::Key, down: bool, st: &mut HotkeyState) -> bool {
    control::hotkey_fired(k, down, st)
}

/// The metrics this machine advertises in `Message::Hello`: its own local screens' bounding box
/// (in **logical** units — points on a Retina Mac, physical pixels once a Windows process is DPI
/// aware) and the UI scale of that coordinate space. The scale is what lets the primary convert
/// its own mouse deltas into this machine's units so the cursor tracks at the same speed.
fn hello_metrics(own: &Layout) -> (u32, u32, f32) {
    let (w, h) = match own.local_bbox() {
        Some((l, t, r, b)) => ((r - l).max(1.0) as u32, (b - t).max(1.0) as u32),
        None => (1920, 1080),
    };
    let scale = own
        .screens
        .iter()
        .find(|s| s.is_local)
        .map(|s| s.scale)
        .unwrap_or(1.0);
    (w, h, scale)
}

/// Build the primary's initial layout from the machine's real displays.
///
/// On macOS this enumerates every attached screen via `display-info`, placing each at its true
/// virtual-desktop position (each is `is_local = true`) so a multi-monitor Mac roams between them
/// natively. On other platforms, or if enumeration fails, we fall back to a single 1080p screen at
/// the origin. Remote (secondary) screens are added later as peers connect.
fn detect_primary_layout(primary_name: &str) -> Layout {
    #[cfg(target_os = "macos")]
    {
        match display_info::DisplayInfo::all() {
            Ok(displays) if !displays.is_empty() => {
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
                        scale: disp.scale_factor,
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
            Err(e) => log::warn!(
                "display enumeration failed ({}); falling back to single screen",
                e
            ),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = primary_name;
        if let Ok((w, h)) = rdev::display_size() {
            return Layout {
                screens: vec![crate::layout::Screen {
                    name: primary_name.to_string(),
                    ox: 0,
                    oy: 0,
                    w: w as u32,
                    h: h as u32,
                    is_local: true,
                    scale: 1.0,
                }],
            };
        }
    }
    Layout {
        screens: vec![crate::layout::Screen {
            name: primary_name.to_string(),
            ox: 0,
            oy: 0,
            w: 1920,
            h: 1080,
            is_local: true,
            scale: 1.0,
        }],
    }
}
