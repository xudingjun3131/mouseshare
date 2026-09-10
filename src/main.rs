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

// `objc::msg_send!` expands into a private helper macro that is not `#[macro_export]`-ed, so we
// have to bring all of its macros into scope with `#[macro_use]` before clipfile.rs (macOS only)
// can use them. The Windows / Linux builds don't pull objc in, so gate the extern crate to macOS.
#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

mod app;
mod capture;
mod clipboard;
mod clipfile;
mod config;
mod control;
mod diag;
mod discovery;
mod i18n;
mod input;
mod layout;
mod network;
mod protocol;
mod single_instance;
mod transfer;
#[cfg(target_os = "windows")]
mod tray;
mod ui;

// `app.rs` historically referenced `crate::Ctrl`; keep that path working after the move.
pub use control::Ctrl;

use log::info;

use crate::config::{load_config, save_config, Config};
use crate::control::{
    cycle_control, on_enter_screen, on_leave_screen, on_secondary_input, return_control,
    CaptureMode, GrabCtx, HotkeyState,
};
use crate::i18n::Lang;
use crate::layout::{Layout, PanelSpec, Screen};
use crate::network::{connect_client, start_hub, Net};
use crate::protocol::Message;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

    // Must happen before any window exists: hiding/minimising the window is exactly when macOS
    // would otherwise put us into App Nap and start throttling the event tap.
    capture::disable_app_nap();

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
    // Namespace every file-transfer token with this machine's name. The hub reassembles every
    // peer's copies through one receiver keyed by token, and relayed copies keep the sender's
    // token, so two machines numbering their copies from 1 would collide (see `transfer`).
    transfer::set_machine_name(&my_name);
    // Background threads (file transfer) need a language for their notifications; mirror the
    // configured one once here so they never have to touch the GUI.
    crate::i18n::set_lang(Lang::from_code(&config.lang));

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
    //
    // On the **primary**, the local panels are always re-detected from the OS (they are a fact, not
    // a preference), but the *remote* panels are restored from the saved config. That merge is what
    // lets the user park a client on the left / above / below and have it stay there: rebuilding the
    // layout from scratch every launch used to drop every remote tile back to the right edge, so
    // left/up/down crossing silently stopped working after a restart.
    let layout: Arc<Mutex<Layout>> = Arc::new(Mutex::new(if mode == "primary" {
        let mut l = detect_primary_layout(&primary_name);
        for s in config.layout.screens.iter().filter(|s| !s.is_local) {
            if !l.screens.iter().any(|x| x.name == s.name) {
                l.screens.push(s.clone());
            }
        }
        l
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
    // Forwarded-delta speed: the ratio itself is derived per hand-off from both machines' scale
    // factors; this only seeds the user's manual trim from the saved config.
    control::set_motion_scale(config.motion_scale);
    // What we advertise to the hub: our local screens' bounding box (logical units) plus the UI
    // scale of that coordinate space, so the primary can normalise forwarded mouse deltas.
    let (my_w, my_h, my_scale, my_panels) = hello_panels(&own_layout);

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
                    panels: my_panels.clone(),
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
        local_screens: own_layout
            .screens
            .iter()
            .filter(|s| s.is_local)
            .cloned()
            .collect(),
        layout_snap: Arc::new(layout.lock().unwrap().clone()),
        ..Default::default()
    }));
    // Forwarded input is handed to the network through an unbounded queue instead of being sent
    // inline: the producer is the macOS event-tap callback, which must never wait on the `net`
    // lock (a stalled tap is disabled by the OS, and a disabled tap stops dropping events while
    // we keep forwarding — both cursors then move at once). See `GrabCtx::input_tx`.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<control::OutboundInput>();
    {
        let net = net.clone();
        std::thread::spawn(move || {
            for cmd in input_rx {
                net.lock().unwrap().send_input(&cmd.target, cmd.ev);
            }
        });
    }

    let grab_ctx: Arc<GrabCtx> = Arc::new(GrabCtx {
        net: net.clone(),
        layout: layout.clone(),
        ctrl: ctrl.clone(),
        mode: Mutex::new(CaptureMode::Local),
        my_name: my_name.clone(),
        primary_name: primary_name.clone(),
        input_tx: Some(input_tx),
        ui_window_rect: Mutex::new(None),
    });

    // Shared clipboard state. It records the last value *we* put on the local clipboard (from
    // a remote machine or from a local write) so the monitor can tell a genuine new copy from
    // our own echo — without it the two machines bounce one clipboard update back and forth
    // forever.
    let clip_state: Arc<Mutex<clipboard::ClipState>> =
        Arc::new(Mutex::new(clipboard::ClipState::default()));

    // Reassembly state for incoming file transfers (one per machine).
    let file_rx: Arc<Mutex<transfer::Receiver>> =
        Arc::new(Mutex::new(transfer::Receiver::default()));

    // The event-tap callback runs inside macOS's input pipeline and must never block on a mutex
    // the GUI thread might be holding — a stalled tap is silently disabled, and a disabled tap
    // stops dropping events (both cursors move at once). So the control plane reads a snapshot
    // of the layout (and of who is reachable) that this thread refreshes a few times a second.
    {
        let layout = layout.clone();
        let ctrl = ctrl.clone();
        let net_snap = net.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(400));
            let snap = Arc::new(layout.lock().unwrap().clone());
            let live = Arc::new(net_snap.lock().unwrap().peer_names());
            let mut c = ctrl.lock().unwrap();
            c.layout_snap = snap;
            c.live = live;
        });
    }

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
                    format!(
                        "{}({}x{}@{},{} local={})",
                        s.name, s.w, s.h, s.ox, s.oy, s.is_local
                    )
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
        let clip_state = clip_state.clone();
        let file_rx = file_rx.clone();
        std::thread::spawn(move || {
            for (from, msg) in inc_rx {
                match msg {
                    Message::Clipboard { text } => {
                        if mode2 == "secondary" {
                            clipboard::apply_remote_text(&clip_state, &text);
                        } else {
                            // Hub: mirror it locally and pass it on to every other peer.
                            clipboard::relay(&clip_state, &net, &text, &from);
                        }
                    }
                    // A file copy arriving from another machine: reassemble, then put the paths
                    // on the local pasteboard so Cmd/Ctrl+V pastes them. On the hub the copy is
                    // ALSO relayed to every other peer — the sender's `broadcast` only reaches
                    // the hub's reader, not the other clients.
                    Message::ClipboardFiles { .. }
                    | Message::FileChunk { .. }
                    | Message::FileEnd { .. } => {
                        if mode2 == "primary" {
                            net.lock().unwrap().broadcast_all_except(msg.clone(), &from);
                        }
                        let finished = file_rx.lock().unwrap().handle(msg);
                        if let Some(paths) = finished {
                            let n = paths.len();
                            if clipboard::apply_remote_files(&clip_state, &paths) {
                                crate::app::notify(crate::i18n::tr_file_received(n));
                                crate::diag::log(&format!(
                                    "FILE-APPLIED n={} first={}",
                                    n,
                                    paths
                                        .first()
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_default()
                                ));
                            } else {
                                // Reassembly succeeded but the pasteboard write did not — without
                                // this line the copy just silently does nothing.
                                crate::diag::log(&format!(
                                    "FILE-APPLY-FAILED n={} (pasteboard write rejected the paths)",
                                    n
                                ));
                                crate::app::notify(crate::i18n::tr_file_apply_failed(n));
                            }
                        }
                    }
                    Message::Input(ev) => {
                        // Only a secondary applies forwarded input (it is being driven).
                        if mode2 == "secondary" {
                            on_secondary_input(&grab_ctx, ev);
                        }
                    }
                    Message::Hello {
                        name,
                        width,
                        height,
                        scale,
                        panels,
                    } => {
                        if mode2 == "primary" {
                            // A peer that predates the panel list only reports a bounding box;
                            // model that as the single panel it is.
                            let specs: Vec<PanelSpec> = if panels.is_empty() {
                                vec![PanelSpec {
                                    name: name.clone(),
                                    ox: 0,
                                    oy: 0,
                                    w: width,
                                    h: height,
                                    scale,
                                }]
                            } else {
                                panels
                            };
                            if layout.lock().unwrap().ensure_host(&name, &specs) {
                                info!("registered {} panel(s) for peer {}", specs.len(), name);
                            }
                        }
                    }
                    Message::Layout { layout: new_layout } => {
                        if mode2 == "secondary" {
                            *layout.lock().unwrap() = new_layout;
                        }
                    }
                    Message::EnterScreen {
                        side,
                        fx,
                        fy,
                        panel,
                    } => {
                        if mode2 == "secondary" {
                            on_enter_screen(&grab_ctx, side, fx, fy, panel);
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
        info!(
            "discovery beacon broadcasting on udp/{}",
            discovery::DISCOVERY_PORT
        );
    } else {
        let net_d = net.clone();
        let inc_tx_d = inc_tx.clone();
        let discovered_d = discovered.clone();
        let my_name_d = my_name.clone();
        let my_panels_d = my_panels.clone();
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
                        panels: my_panels_d.clone(),
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
        // The monitor owns the shared clipboard state: every "fresh" copy (text or files) it sees
        // is pushed to the peers and recorded, so a value that came from the other machine — or
        // that we just wrote ourselves — is not echoed back.
        clipboard::start_monitor(clip_state.clone(), net.clone());
    }

    // ---- Capture ----
    // Shared "permission denied" flag: set by the capture thread when it cannot create the event
    // tap, polled by the GUI so it can pop the native prompt + a guidance dialog with re-check.
    let capture_failed: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    if mode == "primary" {
        // Native grab: CGEventTap on macOS; rdev observer (legacy) elsewhere.
        capture::start_capture(grab_ctx.clone(), capture_failed.clone());
    } else {
        info!(
            "running as secondary; waiting for input from {}",
            server_addr
        );
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
    //
    // `with_titlebar_shown(false)` is what actually makes the title bar *transparent* (it maps
    // to `NSWindow.titlebarAppearsTransparent`). Without it macOS paints an opaque title-bar
    // strip over the top ~28 pt of the window, which covers the upper-right corner — exactly
    // where the language toggle / status pill live — and clips them to a sliver. The toolbar
    // already reserves a left inset for the traffic lights and a right/top inset for the
    // rounded corner, so with a transparent title bar everything stays visible.
    #[cfg(target_os = "macos")]
    {
        viewport = viewport
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
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
            ui::setup_fonts(&cc.egui_ctx);
            ui::setup_style(&cc.egui_ctx);
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

/// The payload this machine advertises in `Message::Hello`: its own screens' bounding box (in
/// **logical** units — points on a Retina Mac, physical pixels once a Windows process is DPI
/// aware), the UI scale of that coordinate space, and the full panel list.
///
/// The scale is what lets the primary convert its own mouse deltas into this machine's units so the
/// cursor tracks at the same speed. The panel list is what lets the primary model a multi-monitor
/// machine faithfully instead of approximating it with a bounding box full of dead space. Panel
/// offsets are rebased onto the group's own top-left so the hub can anchor the machine anywhere.
pub fn hello_panels(own: &Layout) -> (u32, u32, f32, Vec<PanelSpec>) {
    let locals: Vec<&Screen> = own.screens.iter().filter(|s| s.is_local).collect();
    let bbox = own.local_bbox();
    let (w, h) = match bbox {
        Some((l, t, r, b)) => ((r - l).max(1.0) as u32, (b - t).max(1.0) as u32),
        None => (1920, 1080),
    };
    let scale = locals.first().map(|s| s.scale).unwrap_or(1.0);
    let (ox, oy) = bbox
        .map(|b| (b.0.round() as i32, b.1.round() as i32))
        .unwrap_or((0, 0));
    let panels = locals
        .iter()
        .map(|s| PanelSpec {
            name: s.name.clone(),
            ox: s.ox - ox,
            oy: s.oy - oy,
            w: s.w,
            h: s.h,
            scale: s.scale,
        })
        .collect();
    (w, h, scale, panels)
}

/// Name every detected display: the OS's main display keeps the bare machine name, the rest are
/// numbered `#2`, `#3`, … in detection order.
///
/// The bare name goes to the **main** display, not to whichever display happens to sort first.
/// A laptop whose external monitor sits to the left of (or above) the built-in panel sorts ahead
/// of it, and the old `disp.is_primary || i == 0` rule then gave *both* of them the machine name.
/// Two panels sharing one name is not cosmetic: egui derives each tile's interaction id from the
/// name (dragging one would drag the other), `Screen::host` falls back to `name` so one panel
/// would claim a machine that does not exist, and the client's `crossing_back` panel lookup
/// becomes ambiguous — the cursor would be unable to come back from one of the two displays.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn name_panels(machine: &str, is_main: &[bool]) -> Vec<String> {
    let main = is_main.iter().position(|m| *m).unwrap_or(0);
    let mut extra = 0;
    (0..is_main.len())
        .map(|i| {
            if i == main {
                machine.to_string()
            } else {
                extra += 1;
                format!("{} #{}", machine, extra + 1)
            }
        })
        .collect()
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
                let flags: Vec<bool> = d.iter().map(|disp| disp.is_primary).collect();
                let names = name_panels(primary_name, &flags);
                let mut screens = Vec::with_capacity(d.len());
                for (disp, name) in d.iter().zip(names) {
                    screens.push(Screen {
                        name,
                        host: primary_name.to_string(),
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
                screens: vec![Screen {
                    name: primary_name.to_string(),
                    host: primary_name.to_string(),
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
        screens: vec![Screen {
            name: primary_name.to_string(),
            host: primary_name.to_string(),
            ox: 0,
            oy: 0,
            w: 1920,
            h: 1080,
            is_local: true,
            scale: 1.0,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::name_panels;

    /// The bare machine name belongs to the **main** display, whatever order the displays happen
    /// to sort in.
    ///
    /// The old rule (`disp.is_primary || i == 0`) handed the machine name to the first display in
    /// the list *and* to the OS's main display. A laptop with an external monitor to its left sorts
    /// the external first, so both displays ended up named after the machine — one egui interaction
    /// id for two tiles (dragging one moved the other), one panel claiming a machine that does not
    /// exist, and an ambiguous `crossing_back` panel lookup on the client.
    #[test]
    fn names_are_unique_whatever_the_display_order() {
        // External monitor (x < 0) sorts first; the built-in panel is the main display.
        assert_eq!(name_panels("mac", &[false, true]), ["mac #2", "mac"]);
        // The everyday case: the main display comes first.
        assert_eq!(name_panels("mac", &[true, false]), ["mac", "mac #2"]);
        // Main display in the middle of three.
        assert_eq!(
            name_panels("mac", &[false, true, false]),
            ["mac #2", "mac", "mac #3"]
        );
        // No display claims to be main (enumeration quirk): the first stands in.
        assert_eq!(name_panels("mac", &[false, false]), ["mac", "mac #2"]);
        // Single display.
        assert_eq!(name_panels("mac", &[true]), ["mac"]);
    }

    #[test]
    fn names_never_collide_for_any_display_count() {
        for n in 1..=5usize {
            for main in 0..n {
                let mut flags = vec![false; n];
                flags[main] = true;
                let names = name_panels("m", &flags);
                assert_eq!(names.len(), n);
                assert_eq!(names[main], "m", "the main display keeps the bare name");
                let mut uniq = names.clone();
                uniq.sort();
                uniq.dedup();
                assert_eq!(uniq.len(), n, "duplicate names for {flags:?}: {names:?}");
            }
        }
    }
}
