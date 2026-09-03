//! egui window: configuration + the draggable multi-machine screen layout.

use crate::config::{save_config, Config};
use crate::layout::Layout;
use crate::network::Net;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Id, Rect, Sense};
use std::sync::{Arc, Mutex};

pub struct MouseShareApp {
    pub config: Config,
    pub shared_layout: Arc<Mutex<Layout>>,
    pub net: Arc<Mutex<Net>>,
    pub my_name: String,
}

impl MouseShareApp {
    pub fn new(config: Config, shared_layout: Arc<Mutex<Layout>>, net: Arc<Mutex<Net>>, my_name: String) -> Self {
        Self {
            config,
            shared_layout,
            net,
            my_name,
        }
    }
}

impl eframe::App for MouseShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::SidePanel::left("config").show(ctx, |ui| {
            ui.heading("MouseShare");
            ui.label("Share mouse, keyboard & clipboard over LAN.");
            ui.separator();

            ui.label("This machine's name (unique):");
            ui.text_edit_singleline(&mut self.config.name);

            ui.label("Role:");
            ui.radio_value(&mut self.config.mode, "primary".to_string(), "Primary (server, has the real mouse/keyboard)");
            ui.radio_value(&mut self.config.mode, "secondary".to_string(), "Secondary (receives input)");

            if self.config.mode == "secondary" {
                ui.label("Primary address (host:port):");
                ui.text_edit_singleline(&mut self.config.server_addr);
            } else {
                ui.label("Listen port:");
                ui.add(egui::DragValue::new(&mut self.config.port).speed(1));
                if ui.button("Detect my LAN IP").clicked() {
                    if let Ok(ip) = local_ip_address::local_ip() {
                        self.config.server_addr = format!("{}:{}", ip, self.config.port);
                    }
                }
            }

            ui.separator();
            ui.label("Primary machine name (must match that machine's name):");
            ui.text_edit_singleline(&mut self.config.primary_name);

            ui.separator();
            if ui.button("Save config").clicked() {
                self.config.layout = self.shared_layout.lock().unwrap().clone();
                save_config(&self.config);
            }
            ui.label("Saved. Restart the app for role/network changes to take effect.");

            ui.separator();
            let peers = self.net.lock().unwrap().peer_count();
            ui.label(format!("Connected peers: {}", peers));
            ui.label(format!("Local name: {}", self.my_name));
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Screen layout — drag a screen to reposition it");
            ui.label("Place screens the way they sit on your desk. The primary (highlighted) is where your real cursor lives; cross an edge to hand control to a neighbour.");
            ui.separator();

            let mut layout = self.shared_layout.lock().unwrap();
            if layout.screens.is_empty() {
                layout.screens.push(crate::layout::Screen {
                    name: self.config.name.clone(),
                    ox: 0,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                });
            }

            if ui.button("+ Add screen to the right").clicked() {
                let max_x = layout
                    .screens
                    .iter()
                    .map(|s| s.ox + s.w as i32)
                    .max()
                    .unwrap_or(0);
                let n = layout.screens.len() + 1;
                layout.screens.push(crate::layout::Screen {
                    name: format!("machine-{}", n),
                    ox: max_x + 40,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                });
            }

            ui.separator();
            draw_layout(ui, &mut layout, &self.config.primary_name, &self.config.name);
        });
    }
}

fn draw_layout(ui: &mut egui::Ui, layout: &mut Layout, primary_name: &str, my_name: &str) {
    let avail = ui.available_size();
    if avail.x < 10.0 || avail.y < 10.0 {
        return;
    }

    let (minx, miny, maxx, maxy) = bounds(layout);
    let vw = (maxx - minx).max(1) as f32;
    let vh = (maxy - miny).max(1) as f32;
    let pad = 60.0;
    let scale = ((avail.x - pad * 2.0) / vw).min((avail.y - pad * 2.0) / vh).max(0.05);
    let offx = (avail.x - vw * scale) / 2.0 - minx as f32 * scale;
    let offy = (avail.y - vh * scale) / 2.0 - miny as f32 * scale;

    for s in layout.screens.iter_mut() {
        let x = offx + s.ox as f32 * scale;
        let y = offy + s.oy as f32 * scale;
        let w = s.w as f32 * scale;
        let h = s.h as f32 * scale;
        let rect = Rect::from_min_size(pos2(x, y), vec2(w, h));

        let is_primary = s.name == primary_name;
        let is_me = s.name == my_name;
        let resp = ui.interact(rect, Id::new(("screen", &s.name)), Sense::drag());
        if resp.dragged() {
            let d = resp.drag_delta();
            s.ox += (d.x / scale) as i32;
            s.oy += (d.y / scale) as i32;
        }

        let fill = if is_primary {
            Color32::from_rgb(70, 120, 255)
        } else if is_me {
            Color32::from_rgb(80, 180, 120)
        } else {
            Color32::from_rgb(110, 110, 120)
        };
        ui.painter().rect_filled(rect, 6.0, fill);
        ui.painter().rect_stroke(rect, 6.0, (2.0, Color32::WHITE));

        let title = if is_primary { format!("★ {}", s.name) } else { s.name.clone() };
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            &title,
            FontId::proportional(16.0),
            Color32::WHITE,
        );
        ui.painter().text(
            pos2(rect.center().x, rect.center().y + 22.0),
            Align2::CENTER_CENTER,
            &format!("{}×{}", s.w, s.h),
            FontId::proportional(12.0),
            Color32::from_gray(220),
        );
    }

    // Draw a little hint about edges.
    ui.label("Tip: align screen edges so the cursor can cross directly from one to the next.");
}

fn bounds(layout: &Layout) -> (i32, i32, i32, i32) {
    let mut minx = i32::MAX;
    let mut miny = i32::MAX;
    let mut maxx = i32::MIN;
    let mut maxy = i32::MIN;
    for s in &layout.screens {
        minx = minx.min(s.ox);
        miny = miny.min(s.oy);
        maxx = maxx.max(s.ox + s.w as i32);
        maxy = maxy.max(s.oy + s.h as i32);
    }
    if minx == i32::MAX {
        (0, 0, 1920, 1080)
    } else {
        (minx, miny, maxx, maxy)
    }
}
