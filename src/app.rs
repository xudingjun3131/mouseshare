//! egui window: configuration + the draggable multi-machine screen layout.
//!
//! UI conventions:
//! * All user-facing text comes from `crate::i18n` (Chinese / English, toggled in the title bar
//!   and persisted in `Config.lang`).
//! * The left side panel holds grouped setting cards; the central panel is a dark canvas where
//!   the virtual desktop is laid out.

use crate::clipboard;
use crate::config::{save_config, Config};
use crate::i18n::{tr, Lang, Tr};
use crate::layout::Layout;
use crate::network::Net;
use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Id, Rect, Sense};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---- Central-canvas palette (fixed dark, independent of the egui theme) ----
const CANVAS_BG: Color32 = Color32::from_rgb(26, 28, 36);
const COL_PRIMARY: Color32 = Color32::from_rgb(64, 118, 255);
const COL_ME: Color32 = Color32::from_rgb(46, 184, 114);
const COL_OTHER: Color32 = Color32::from_rgb(88, 96, 116);
const CANVAS_TEXT: Color32 = Color32::from_gray(235);
const CANVAS_MUTED: Color32 = Color32::from_gray(165);

/// Install a CJK fallback font so Chinese/Japanese/Korean glyphs render instead of tofu boxes.
///
/// `default_fonts` (the only font feature we use on macOS/Windows) ships ProggyClean, which is
/// ASCII-only. Without this, every non-Latin character in the UI shows as ▢▢▢.
///
/// Strategy: prefer an **embedded** Noto Sans SC OTF (the only thing we can guarantee across
/// every user's machine, including CI containers, Windows boxes without East Asian language
/// packs, and Linux distros with no CJK package installed). If loading the embedded font fails
/// for some reason, fall back to common system fonts.
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // First try: the bundled OTF (always present, identical on every machine, no surprises).
    let embedded: &[u8] = include_bytes!("../resources/NotoSansSC-Regular.otf");
    if !embedded.is_empty() {
        log::info!("using bundled Noto Sans SC ({} KB)", embedded.len() / 1024);
        fonts
            .font_data
            .insert("cjk".into(), egui::FontData::from_owned(embedded.to_vec()));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("cjk".into());
        }
        ctx.set_fonts(fonts);
        return;
    }

    // Fallback: scan well-known system locations for any CJK-capable font. Note that
    // ab_glyph can only read single-file TTF/OTF — TTC collections (Hiragino Sans GB.ttc,
    // msyh.ttc, …) do NOT parse their faces, so they are listed last.
    let candidates: &[&str] = &[
        // macOS
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/CJKSymbolsFallback.ttc",
        // Windows
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/msyhbd.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttf",
        "C:/Windows/Fonts/simfang.ttf",
        "C:/Windows/Fonts/simkai.ttf",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttf",
        "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/TTF/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    ];

    for path in candidates {
        match std::fs::read(path) {
            Ok(bytes) => {
                log::info!("loaded CJK font from {} ({} KB)", path, bytes.len() / 1024);
                fonts
                    .font_data
                    .insert("cjk".into(), egui::FontData::from_owned(bytes));
                for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts.families.entry(fam).or_default().push("cjk".into());
                }
                break;
            }
            Err(_) => continue,
        }
    }

    ctx.set_fonts(fonts);
}

/// Global look & feel: roomier spacing, chunkier buttons. Colors follow the system theme;
/// only the layout canvas is fixed-dark (see `CANVAS_BG`).
pub fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = vec2(8.0, 8.0);
    style.spacing.button_padding = vec2(12.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    ctx.set_style(style);
}

pub struct MouseShareApp {
    pub config: Config,
    pub shared_layout: Arc<Mutex<Layout>>,
    pub net: Arc<Mutex<Net>>,
    pub my_name: String,
    /// Selected UI language (mirrors `config.lang`, kept separate for cheap access).
    pub lang: Lang,
    /// Transient toast message with the moment it was shown (auto-hides after 3 s).
    pub toast: Option<(Instant, String)>,
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
        let lang = Lang::from_code(&config.lang);
        Self {
            config,
            shared_layout,
            net,
            my_name,
            lang,
            toast: None,
            startup_error,
        }
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((Instant::now(), msg.into()));
    }
}

impl eframe::App for MouseShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t = tr(self.lang);

        // Expire the transient toast.
        if let Some((at, _)) = &self.toast {
            if at.elapsed() > Duration::from_secs(3) {
                self.toast = None;
            }
        }

        // ---- Title bar: brand + language toggle ----
        egui::TopBottomPanel::top("titlebar").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("MouseShare").heading().strong());
                ui.label(egui::RichText::new(t.tagline).weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(egui::RichText::new(self.lang.toggle_label()).strong())
                        .clicked()
                    {
                        self.lang = self.lang.toggled();
                        self.config.lang = self.lang.code().to_string();
                        save_config(&self.config); // persist immediately
                    }
                });
            });
            ui.add_space(8.0);
        });

        // ---- Startup failure banner (network error at boot) ----
        if self.startup_error.is_some() {
            let err = self.startup_error.clone().unwrap();
            egui::TopBottomPanel::top("startup_error").show(ctx, |ui| {
                egui::Frame::none()
                    .fill(Color32::from_rgb(70, 32, 36))
                    .inner_margin(egui::Margin::same(10.0))
                    .rounding(egui::Rounding::same(6.0))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(t.err_title)
                                    .strong()
                                    .color(Color32::from_rgb(255, 150, 150)),
                            );
                            ui.label(
                                egui::RichText::new(err).color(Color32::from_rgb(255, 205, 205)),
                            );
                        });
                        ui.label(
                            egui::RichText::new(t.err_hint).small().color(CANVAS_MUTED),
                        );
                    });
            });
        }

        // ---- Left panel: grouped setting cards ----
        egui::SidePanel::left("config")
            .default_width(340.0)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.basic_card(ui, t);
                        self.screens_card(ui, t);
                        self.status_card(ui, t);
                    });
            });

        // ---- Central panel: dark canvas with the virtual desktop ----
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(CANVAS_BG))
            .show(ctx, |ui| {
                ui.style_mut().visuals.override_text_color = Some(CANVAS_TEXT);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(egui::RichText::new(t.layout_title).heading().strong());
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(
                        egui::RichText::new(t.layout_hint)
                            .small()
                            .color(CANVAS_MUTED),
                    );
                });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    legend_chip(ui, COL_PRIMARY, t.legend_primary);
                    legend_chip(ui, COL_ME, t.legend_me);
                    legend_chip(ui, COL_OTHER, t.legend_client);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(16.0);
                        ui.label(
                            egui::RichText::new(t.layout_tip)
                                .small()
                                .color(CANVAS_MUTED),
                        );
                    });
                });
                ui.add_space(4.0);

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
                draw_layout(ui, &mut layout, &self.config.primary_name, &self.config.name, t);
            });
    }
}

impl MouseShareApp {
    fn basic_card(&mut self, ui: &mut egui::Ui, t: Tr) {
        ui.add_space(4.0);
        egui::Frame::none()
            .fill(ui.visuals().extreme_bg_color)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::same(12.0))
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(t.section_basic).strong().heading());

                ui.label(egui::RichText::new(t.machine_name).weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.name)
                        .desired_width(f32::INFINITY),
                );

                ui.add_space(4.0);
                ui.label(egui::RichText::new(t.role).weak());
                ui.radio_value(
                    &mut self.config.mode,
                    "primary".to_string(),
                    t.role_primary,
                );
                ui.radio_value(
                    &mut self.config.mode,
                    "secondary".to_string(),
                    t.role_secondary,
                );

                if self.config.mode == "secondary" {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(t.server_addr).weak());
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.server_addr)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                } else {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(t.listen_port).weak());
                        ui.add(egui::DragValue::new(&mut self.config.port).speed(1));
                    });
                    if ui.button(t.detect_ip).clicked() {
                        if let Ok(ip) = local_ip_address::local_ip() {
                            self.config.server_addr =
                                format!("{}:{}", ip, self.config.port);
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(t.address).weak());
                        ui.monospace(&self.config.server_addr);
                    });
                    if ui.button(t.copy_addr).clicked() {
                        clipboard::set_clipboard(&self.config.server_addr);
                        self.show_toast(t.copied);
                    }
                }

                ui.separator();
                ui.label(egui::RichText::new(t.primary_name).weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.primary_name)
                        .desired_width(f32::INFINITY),
                );

                ui.add_space(6.0);
                if ui
                    .add_sized(
                        [ui.available_width(), 34.0],
                        egui::Button::new(egui::RichText::new(t.save).strong()),
                    )
                    .clicked()
                {
                    self.config.layout = self.shared_layout.lock().unwrap().clone();
                    save_config(&self.config);
                    self.show_toast(t.saved_hint);
                }

                if let Some((_, msg)) = &self.toast {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(msg.clone())
                            .color(Color32::from_rgb(70, 180, 110)),
                    );
                }
            });
        ui.add_space(4.0);
    }

    fn screens_card(&mut self, ui: &mut egui::Ui, t: Tr) {
        egui::Frame::none()
            .fill(ui.visuals().extreme_bg_color)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::same(12.0))
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(t.section_screens).strong().heading());
                ui.label(egui::RichText::new(t.screens_hint).weak().small());

                let mut layout = self.shared_layout.lock().unwrap();
                let mut dup_idx: Option<usize> = None;
                let mut del_idx: Option<usize> = None;
                for (i, s) in layout.screens.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.monospace(format!("{}  {}×{}", s.name, s.w, s.h));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button(t.del).clicked() {
                                    del_idx = Some(i);
                                }
                                if ui.small_button(t.dup).clicked() {
                                    dup_idx = Some(i);
                                }
                            },
                        );
                    });
                }
                if let Some(i) = dup_idx {
                    layout.duplicate_screen(i);
                }
                if let Some(i) = del_idx {
                    if layout.screens.len() > 1 {
                        layout.screens.remove(i);
                    } else {
                        // Direct field write (not a method call): the layout guard still
                        // borrows self.shared_layout below, so &mut self is unavailable.
                        self.toast = Some((Instant::now(), t.keep_one.to_string()));
                    }
                }

                if ui.button(t.add_screen).clicked() {
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
            });
        ui.add_space(4.0);
    }

    fn status_card(&mut self, ui: &mut egui::Ui, t: Tr) {
        egui::Frame::none()
            .fill(ui.visuals().extreme_bg_color)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::same(12.0))
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(t.section_status).strong().heading());
                let peers = self.net.lock().unwrap().peer_count();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t.peers).weak());
                    ui.label(
                        egui::RichText::new(format!("{}", peers))
                            .strong()
                            .heading(),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t.local_name).weak());
                    ui.monospace(&self.my_name);
                });
                if let Some((_, msg)) = &self.toast {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(msg.clone())
                            .color(Color32::from_rgb(70, 180, 110)),
                    );
                }
            });
        ui.add_space(4.0);
    }
}

fn legend_chip(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.label(egui::RichText::new("●").color(color));
    ui.label(egui::RichText::new(text).small().color(CANVAS_MUTED));
}

fn draw_layout(
    ui: &mut egui::Ui,
    layout: &mut Layout,
    primary_name: &str,
    my_name: &str,
    t: Tr,
) {
    let avail = ui.available_size();
    if avail.x < 40.0 || avail.y < 40.0 {
        return;
    }

    let (minx, miny, maxx, maxy) = bounds(layout);
    let vw = (maxx - minx).max(1) as f32;
    let vh = (maxy - miny).max(1) as f32;
    let pad = 48.0;
    let scale = ((avail.x - pad * 2.0) / vw)
        .min((avail.y - pad * 2.0) / vh)
        .max(0.05);
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
        let resp = if resp.hovered() {
            resp.on_hover_cursor(CursorIcon::Grab)
        } else {
            resp
        };

        let fill = if is_primary {
            COL_PRIMARY
        } else if is_me {
            COL_ME
        } else {
            COL_OTHER
        };
        let stroke = if resp.hovered() || resp.dragged() {
            Color32::from_rgba_unmultiplied(255, 255, 255, 220)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 120)
        };

        // Soft drop shadow, then the card itself.
        ui.painter().rect_filled(
            rect.translate(vec2(0.0, 5.0)),
            10.0,
            Color32::from_black_alpha(110),
        );
        ui.painter().rect_filled(rect, 10.0, fill);
        ui.painter().rect_stroke(rect, 10.0, (1.5, stroke));

        if w > 56.0 && h > 40.0 {
            let title = if is_primary {
                format!("★ {}", s.name)
            } else {
                s.name.clone()
            };
            let cy = rect.center().y;
            let title_y = if h > 76.0 { cy - 12.0 } else { cy };
            ui.painter().text(
                pos2(rect.center().x, title_y),
                Align2::CENTER_CENTER,
                &title,
                FontId::proportional(16.0),
                Color32::WHITE,
            );
            if h > 76.0 {
                ui.painter().text(
                    pos2(rect.center().x, cy + 12.0),
                    Align2::CENTER_CENTER,
                    &format!("{}×{}", s.w, s.h),
                    FontId::proportional(12.0),
                    Color32::from_rgba_unmultiplied(255, 255, 255, 200),
                );
            }
        }
    }

    // Bottom-center hint on the canvas.
    ui.painter().text(
        pos2(avail.x / 2.0, avail.y - 14.0),
        Align2::CENTER_CENTER,
        t.layout_tip,
        FontId::proportional(12.0),
        CANVAS_MUTED,
    );
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
