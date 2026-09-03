//! egui window: configuration + the draggable multi-machine screen layout.

use crate::clipboard;
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
    /// Transient status line (e.g. "已复制连接地址").
    pub copy_status: String,
    /// Set when networking failed at startup (port busy / primary unreachable). Shown as a
    /// banner instead of letting the app exit silently with no window at all.
    pub startup_error: Option<String>,
}

impl MouseShareApp {
    pub fn new(
        config: Config,
        shared_layout: Arc<Mutex<Layout>>,
        net: Arc<Mutex<Net>>,
        my_name: String,
        startup_error: Option<String>,
    ) -> Self {
        Self {
            config,
            shared_layout,
            net,
            my_name,
            copy_status: String::new(),
            startup_error,
        }
    }
}

impl eframe::App for MouseShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Show startup failures prominently. Without this the app used to exit silently and the
        // user saw nothing at all when launching from Finder.
        if let Some(err) = &self.startup_error {
            egui::TopBottomPanel::top("startup_error").show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("⚠ 启动异常：")
                            .strong()
                            .color(Color32::from_rgb(255, 120, 120)),
                    );
                    ui.label(egui::RichText::new(err).color(Color32::from_rgb(255, 190, 190)));
                });
                ui.label(
                    egui::RichText::new("窗口已正常打开。可在左侧修改配置后点击 Save config，然后重启本应用。")
                        .color(Color32::from_gray(210)),
                );
                ui.add_space(4.0);
            });
        }

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
                ui.horizontal(|ui| {
                    ui.label("Address:");
                    ui.monospace(&self.config.server_addr);
                });
                if ui.button("复制连接地址").clicked() {
                    clipboard::set_clipboard(&self.config.server_addr);
                    self.copy_status = "已复制连接地址到剪贴板".to_string();
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
            ui.heading("屏幕 / 客户端");
            ui.label("客户端数量无上限：连上的机器会自动出现在布局里。可在此复制或删除屏幕。");
            {
                let mut layout = self.shared_layout.lock().unwrap();
                let mut dup_idx: Option<usize> = None;
                let mut del_idx: Option<usize> = None;
                for (i, s) in layout.screens.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}  {}×{}", s.name, s.w, s.h));
                        if ui.button("复制").clicked() {
                            dup_idx = Some(i);
                        }
                        if ui.button("删除").clicked() {
                            del_idx = Some(i);
                        }
                    });
                }
                if let Some(i) = dup_idx {
                    layout.duplicate_screen(i);
                }
                if let Some(i) = del_idx {
                    if layout.screens.len() > 1 {
                        layout.screens.remove(i);
                    } else {
                        self.copy_status = "至少保留一块屏幕".to_string();
                    }
                }
            }
            if ui.button("+ 右侧添加屏幕").clicked() {
                let mut layout = self.shared_layout.lock().unwrap();
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
            let peers = self.net.lock().unwrap().peer_count();
            ui.label(format!("Connected peers: {}", peers));
            ui.label(format!("Local name: {}", self.my_name));
            if !self.copy_status.is_empty() {
                ui.label(egui::RichText::new(&self.copy_status).color(Color32::from_rgb(120, 220, 140)));
            }
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
