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
mod input;
mod layout;
mod network;
mod protocol;

use crate::config::{load_config, Config};
use crate::layout::Layout;
use crate::network::{connect_client, start_hub, Net};
use crate::protocol::{InputEvent, Message};
use log::info;
use rdev::{display_size, Button as RdevButton, Event, EventType};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config: Config = load_config();
    let my_name = config.name.clone();
    let mode = config.mode.clone();
    let server_addr = config.server_addr.clone();
    let port = config.port;
    let primary_name = config.primary_name.clone();

    // Incoming channel: (peer name, message).
    let (inc_tx, inc_rx) = channel::<(String, Message)>();

    let net: Arc<Mutex<Net>> = if mode == "primary" {
        start_hub(port, inc_tx)?
    } else {
        let (net, tx) = connect_client(&server_addr, inc_tx)?;
        let (w, h) = display_size().unwrap_or((1920, 1080));
        tx.send(Message::Hello {
            name: my_name.clone(),
            width: w as u32,
            height: h as u32,
        })
        .ok();
        net
    };

    // Shared state used by the capture thread.
    let layout: Arc<Mutex<Layout>> = Arc::new(Mutex::new(config.layout.clone()));
    let ownership: Arc<Mutex<String>> = Arc::new(Mutex::new(primary_name.clone()));
    let vcursor: Arc<Mutex<(f64, f64)>> = Arc::new(Mutex::new((0.0, 0.0)));
    let last_real: Arc<Mutex<(f64, f64)>> = Arc::new(Mutex::new((-1.0, -1.0))); // sentinel: uninitialised

    // ---- Incoming message handler ----
    {
        let net = net.clone();
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
                    Message::Hello { .. } => { /* registration happened at connect time */ }
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
    let gui_app = app::MouseShareApp::new(config, layout, net, my_name);
    let options = eframe::NativeOptions::default();
    let result = eframe::run_native(
        "MouseShare",
        options,
        Box::new(move |_cc| {
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
                let (name, ox, oy) = {
                    let l = layout.lock().unwrap();
                    let s = &l.screens[idx];
                    (s.name.clone(), s.ox as f64, s.oy as f64)
                };
                *ownership.lock().unwrap() = name.clone();
                if name != primary_name {
                    net.lock()
                        .unwrap()
                        .send_input(&name, InputEvent::MouseMove { x: cx - ox, y: cy - oy });
                }
            } else {
                // In a gap: keep forwarding to the current owner (clamped).
                let cur = ownership.lock().unwrap().clone();
                if cur != primary_name {
                    let l = layout.lock().unwrap();
                    if let Some(s) = l.screens.iter().find(|s| s.name == cur) {
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
                *vc = layout.lock().unwrap().clamp(cx, cy);
            }

            // Treadmill: when multiple screens exist and the real cursor is shoved against an
            // edge while still moving outward, warp it to the opposite edge so it keeps producing
            // motion — that motion is what carries the virtual cursor onto the neighbour.
            let recycle = layout.lock().unwrap().screens.len() > 1;
            if recycle && (d.0 != 0.0 || d.1 != 0.0) {
                let (pw, ph) = {
                    let l = layout.lock().unwrap();
                    let s = l.index_of(primary_name).and_then(|i| l.screens.get(i));
                    s.map(|s| (s.w as f64, s.h as f64)).unwrap_or((1920.0, 1080.0))
                };
                let margin = 4.0;
                let near_right = x >= pw - margin;
                let near_left = x <= margin;
                let near_bottom = y >= ph - margin;
                let near_top = y <= margin;
                let pushing_out = (near_right && d.0 > 0.0)
                    || (near_left && d.0 < 0.0)
                    || (near_bottom && d.1 > 0.0)
                    || (near_top && d.1 < 0.0);
                if pushing_out {
                    let nx = if near_right {
                        margin
                    } else if near_left {
                        pw - margin
                    } else {
                        x
                    };
                    let ny = if near_bottom {
                        margin
                    } else if near_top {
                        ph - margin
                    } else {
                        y
                    };
                    input::warp_cursor(nx, ny);
                    *last_real.lock().unwrap() = (nx, ny);
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
