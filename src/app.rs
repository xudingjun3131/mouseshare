//! egui window: configuration + the draggable multi-machine screen layout.
//!
//! UI conventions:
//! * All user-facing text comes from `crate::i18n` (Chinese / English, toggled in the title bar
//!   and persisted in `Config.lang`).
//! * Visual language follows macOS HIG: a toolbar-style title bar with the app glyph, grouped
//!   inset cards in the sidebar, and a soft neutral "Displays" canvas where the virtual desktop
//!   is laid out. One accent color (system blue), hairline separators, generous spacing.
//! * Colors are derived from the egui theme so both light and dark system appearances stay
//!   readable. Screen tiles use solid fills with white labels, so they read on any canvas.

use crate::clipboard;
use crate::config::{save_config, Config};
use crate::i18n::{tr, Lang, Tr};
use crate::layout::Layout;
use crate::network::{connect_client, Net};
use crate::protocol::Message;
use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Id, Rect, Rounding, Sense};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---- Accent + screen role colors (solid, contrast-safe on any canvas) ----
const COL_PRIMARY: Color32 = Color32::from_rgb(0, 122, 255); // macOS system blue
const COL_ME: Color32 = Color32::from_rgb(48, 199, 89); // iOS green
const COL_CLIENT: Color32 = Color32::from_rgb(120, 120, 128); // iOS gray (label-safe)

/// Theme-derived palette. Everything outside the canvas uses the egui theme directly; the canvas
/// and its tiles need explicit colors so they stay legible in both light and dark modes.
#[derive(Clone, Copy)]
struct UiTheme {
    canvas_bg: Color32,
    canvas_text: Color32,
    canvas_muted: Color32,
    accent: Color32,
    hairline: Color32,
}

impl UiTheme {
    fn from_ctx(ctx: &egui::Context) -> Self {
        let dark = ctx.style().visuals.dark_mode;
        if dark {
            UiTheme {
                canvas_bg: Color32::from_rgb(30, 30, 36),
                canvas_text: Color32::from_rgb(235, 235, 240),
                canvas_muted: Color32::from_rgb(150, 150, 157),
                accent: Color32::from_rgb(10, 132, 255),
                hairline: Color32::from_rgba_unmultiplied(255, 255, 255, 22),
            }
        } else {
            UiTheme {
                canvas_bg: Color32::from_rgb(243, 243, 248), // systemGray6
                canvas_text: Color32::from_rgb(60, 60, 67),  // label
                canvas_muted: Color32::from_rgb(142, 142, 147),
                accent: Color32::from_rgb(0, 122, 255),
                hairline: Color32::from_rgba_unmultiplied(0, 0, 0, 12),
            }
        }
    }
}

/// Install a CJK-capable font and make it the **primary** typeface for every language.
///
/// egui's default font (ProggyClean/Ubuntu) is ASCII-only. Without a CJK font, every non-Latin
/// character shows as ▢▢▢. But if we only add the CJK font as a *fallback* (at the end of the
/// family list), the two UI languages end up looking different: English renders in egui's default
/// sans while Chinese falls through to Noto Sans SC. Pushing the CJK font to the **front** of each
/// family makes Latin and CJK glyphs share ONE typeface, so the Chinese and English layouts look
/// identical. Noto Sans SC ships full Latin glyphs, so English is unaffected apart from the
/// consistent look. egui's default font stays behind it as a safety net for any rare missing glyph.
///
/// Strategy: prefer an **embedded** Noto Sans SC OTF (the only thing we can guarantee across
/// every user's machine, including CI containers, Windows boxes without East Asian language
/// packs, and Linux distros with no CJK package installed). If loading the embedded font fails
/// for some reason, fall back to common system fonts.
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Put the CJK font first in every family so both languages share one typeface.
    let prefer_cjk = |fonts: &mut egui::FontDefinitions| {
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = fonts.families.entry(fam).or_default();
            if !list.iter().any(|f| f == "cjk") {
                list.insert(0, "cjk".into());
            }
        }
    };

    // First try: the bundled OTF (always present, identical on every machine, no surprises).
    let embedded: &[u8] = include_bytes!("../resources/NotoSansSC-Regular.otf");
    if !embedded.is_empty() {
        log::info!("using bundled Noto Sans SC ({} KB)", embedded.len() / 1024);
        fonts
            .font_data
            .insert("cjk".into(), egui::FontData::from_owned(embedded.to_vec()));
        prefer_cjk(&mut fonts);
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
                prefer_cjk(&mut fonts);
                break;
            }
            Err(_) => continue,
        }
    }

    ctx.set_fonts(fonts);
}

/// Global look & feel: macOS-flavored metrics — consistent 8px control rounding, roomy spacing.
/// Colors stay theme-driven; the canvas derives its own palette in `UiTheme`.
pub fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = vec2(10.0, 10.0);
    style.spacing.button_padding = vec2(14.0, 7.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.spacing.indent = 14.0;
    // Uniform control rounding across buttons / inputs / radios — the macOS look (squircle-ish).
    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
        &mut style.visuals.widgets.noninteractive,
    ] {
        w.rounding = Rounding::same(8.0);
    }
    style.visuals.window_stroke = egui::Stroke::NONE;
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
    /// Incoming channel used to (re)establish a secondary connection from the GUI.
    pub inc_tx: Sender<(String, Message)>,
    /// Throttle timestamp for the primary's periodic layout push to secondaries.
    pub last_layout_push: Option<Instant>,
    /// The capture thread's control-plane state (who has the mouse, edge-push progress).
    /// Shared read-only here so the status card can show live hand-off state.
    pub ctrl: Arc<Mutex<crate::Ctrl>>,
}

impl MouseShareApp {
    pub fn new(
        config: Config,
        shared_layout: Arc<Mutex<Layout>>,
        net: Arc<Mutex<Net>>,
        my_name: String,
        startup_error: Option<String>,
        inc_tx: Sender<(String, Message)>,
        ctrl: Arc<Mutex<crate::Ctrl>>,
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
            inc_tx,
            last_layout_push: None,
            ctrl,
        }
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((Instant::now(), msg.into()));
    }

    /// (Re)connect to the primary from the running app. Used by the "Connect" button on the
    /// secondary and the "Retry" button on the startup-error banner. Tearing down to `Idle`
    /// first lets the old reader/writer threads stop, then we open a fresh connection and send
    /// Hello. No app restart required.
    fn reconnect(&mut self) {
        let t = tr(self.lang);
        let addr = self.config.server_addr.trim().to_string();
        if addr.is_empty() {
            self.startup_error = Some(self.lang.connect_fail(&addr, "address is empty"));
            return;
        }
        {
            let mut net = self.net.lock().unwrap();
            *net = Net::Idle;
        }
        let n = Arc::new(Mutex::new(Net::Idle));
        match connect_client(&addr, self.inc_tx.clone(), n.clone()) {
            Ok((net_inner, tx)) => {
                let (w, h) = self.screen_size();
                tx.send(Message::Hello {
                    name: self.my_name.clone(),
                    width: w,
                    height: h,
                })
                .ok();
                self.net = net_inner;
                self.startup_error = None;
                self.show_toast(t.connected);
            }
            Err(e) => {
                let msg = self.lang.connect_fail(&addr, &e.to_string());
                self.startup_error = Some(msg.clone());
                self.show_toast(msg);
            }
        }
    }

    /// This machine's real screen size, taken from its own layout entry (falls back to 1080p).
    fn screen_size(&self) -> (u32, u32) {
        let l = self.shared_layout.lock().unwrap();
        l.screens
            .iter()
            .find(|s| s.name == self.my_name)
            .map(|s| (s.w, s.h))
            .unwrap_or((1920, 1080))
    }
}

impl eframe::App for MouseShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t = tr(self.lang);

        // Keep running when the window is closed: sharing (input capture/injection, clipboard,
        // network) lives on background threads that don't need the window. Cancel the close and
        // minimise instead of exiting — the "Quit" button in the status card exits for real.
        // Without this the process died with the window and the user had to keep the window
        // open for sharing to work at all.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        // Expire the transient toast.
        if let Some((at, _)) = &self.toast {
            if at.elapsed() > Duration::from_secs(3) {
                self.toast = None;
            }
        }

        let theme = UiTheme::from_ctx(ctx);

        // ---- Title bar: app glyph + brand + language toggle ----
        egui::TopBottomPanel::top("titlebar").show(ctx, |ui| {
            let panel_rect = ui.max_rect();
            ui.add_space(11.0);
            ui.horizontal(|ui| {
                // App glyph (mouse) drawn in the accent color.
                let (_, icon_rect) = ui.allocate_space(vec2(24.0, 24.0));
                draw_mouse_icon(ui.painter(), icon_rect, theme.accent);

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("MouseShare")
                        .size(17.0)
                        .strong()
                        .color(ui.visuals().strong_text_color()),
                );
                ui.add_space(8.0);
                ui.label(egui::RichText::new(t.tagline).size(12.0).color(ui.visuals().weak_text_color()));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let pill = egui::Button::new(
                        egui::RichText::new(self.lang.toggle_label()).size(12.5),
                    )
                    .rounding(8.0)
                    .fill(ui.visuals().widgets.inactive.bg_fill)
                    .stroke(ui.visuals().widgets.noninteractive.bg_stroke);
                    if ui.add(pill).clicked() {
                        self.lang = self.lang.toggled();
                        self.config.lang = self.lang.code().to_string();
                        save_config(&self.config); // persist immediately
                    }
                });
            });
            ui.add_space(11.0);
            // Hairline under the toolbar.
            ui.painter().line_segment(
                [pos2(panel_rect.left(), panel_rect.bottom()), pos2(panel_rect.right(), panel_rect.bottom())],
                (1.0, theme.hairline),
            );
        });

        // ---- Startup failure banner (network error at boot) ----
        let mut retry_clicked = false;
        if self.startup_error.is_some() {
            let err = self.startup_error.clone().unwrap();
            let is_secondary = self.config.mode == "secondary";
            let retry_label = t.retry_connect;
            egui::TopBottomPanel::top("startup_error").show(ctx, |ui| {
                ui.add_space(10.0);
                egui::Frame::none()
                    .fill(Color32::from_rgb(255, 235, 236))
                    .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                    .rounding(Rounding::same(10.0))
                    .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(255, 200, 202)))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(t.err_title)
                                    .strong()
                                    .color(Color32::from_rgb(196, 30, 44)),
                            );
                            ui.label(
                                egui::RichText::new(err).color(Color32::from_rgb(120, 30, 36)),
                            );
                        });
                        ui.label(
                            egui::RichText::new(t.err_hint)
                                .size(12.0)
                                .color(Color32::from_rgb(150, 90, 95)),
                        );
                        if is_secondary {
                            ui.add_space(6.0);
                            if ui.button(retry_label).clicked() {
                                retry_clicked = true;
                            }
                        }
                    });
                ui.add_space(10.0);
            });
        }
        if retry_clicked {
            self.reconnect();
        }

        // ---- Left sidebar: grouped setting cards ----
        egui::SidePanel::left("config")
            .default_width(360.0)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.basic_card(ui, t, theme);
                        self.screens_card(ui, t);
                        self.status_card(ui, t, theme);
                    });
            });

        // ---- Central canvas: the virtual desktop ----
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme.canvas_bg))
            .show(ctx, |ui| {
                // Paint the header into its own measured block so the canvas rectangle below is
                // exact and does not depend on the fragile cursor state after long hints/legends.
                let header = ui.vertical(|ui| {
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new(t.layout_title)
                                .size(15.0)
                                .strong()
                                .color(theme.canvas_text),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        // Force wrapping in English, where the hint is long enough to overflow a
                        // single line and would otherwise corrupt the following cursor/placement.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(t.layout_hint)
                                    .size(12.5)
                                    .color(theme.canvas_muted),
                            )
                            .wrap(),
                        );
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        legend_chip(ui, COL_PRIMARY, t.legend_primary, theme);
                        ui.add_space(14.0);
                        legend_chip(ui, COL_ME, t.legend_me, theme);
                        ui.add_space(14.0);
                        legend_chip(ui, COL_CLIENT, t.legend_client, theme);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(20.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(t.layout_tip)
                                        .size(12.0)
                                        .color(theme.canvas_muted),
                                )
                                .wrap(),
                            );
                        });
                    });
                    ui.add_space(8.0);
                })
                .response
                .rect;

                let panel_rect = ui.max_rect();
                let canvas_rect = Rect::from_min_max(
                    pos2(panel_rect.min.x, header.max.y),
                    panel_rect.max,
                );

                let mut layout = self.shared_layout.lock().unwrap();
                if layout.screens.is_empty() {
                    layout.screens.push(crate::layout::Screen {
                        name: self.config.name.clone(),
                        ox: 0,
                        oy: 0,
                        w: 1920,
                        h: 1080,
                        is_local: true,
                    });
                }
                let layout_changed = draw_layout(
                    ui,
                    &mut layout,
                    &self.config.name,
                    t,
                    theme,
                    canvas_rect,
                );
                drop(layout);
                // Persist drag repositioning immediately (primary only — it owns the layout and
                // broadcasts it to every secondary within 2 s).
                if layout_changed && self.config.mode == "primary" {
                    self.config.layout = self.shared_layout.lock().unwrap().clone();
                    save_config(&self.config);
                }
            });

        // Primary: push the current layout to every secondary every couple of seconds so all
        // machines draw the same map (including the primary's own screen and any repositioning
        // done in this window). Secondaries adopt it; the primary never receives a Layout.
        if self.config.mode == "primary" {
            let now = Instant::now();
            let due = match self.last_layout_push {
                Some(t0) => now.duration_since(t0) >= Duration::from_secs(2),
                None => true,
            };
            if due {
                let snap = self.shared_layout.lock().unwrap().clone();
                self.net.lock().unwrap().broadcast_layout(&snap);
                self.last_layout_push = Some(now);
            }
        }
    }
}

impl MouseShareApp {
    fn basic_card(&mut self, ui: &mut egui::Ui, t: Tr, theme: UiTheme) {
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            section_header(ui, t.section_basic);

            field_label(ui, t.machine_name, theme);
            ui.add(
                egui::TextEdit::singleline(&mut self.config.name)
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(10.0);
            field_label(ui, t.role, theme);
            ui.radio_value(&mut self.config.mode, "primary".to_string(), t.role_primary);
            ui.radio_value(&mut self.config.mode, "secondary".to_string(), t.role_secondary);

            if self.config.mode == "secondary" {
                ui.add_space(10.0);
                field_label(ui, t.server_addr, theme);
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.server_addr)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                ui.add_space(8.0);
                if ui.button(t.connect_host).clicked() {
                    self.reconnect();
                }
            } else {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    field_label(ui, t.listen_port, theme);
                    ui.add(egui::DragValue::new(&mut self.config.port).speed(1));
                });
                if ui.button(t.detect_ip).clicked() {
                    if let Ok(ip) = local_ip_address::local_ip() {
                        self.config.server_addr = format!("{}:{}", ip, self.config.port);
                    }
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t.address).weak().size(12.0));
                    ui.monospace(&self.config.server_addr);
                });
                if ui.button(t.copy_addr).clicked() {
                    clipboard::set_clipboard(&self.config.server_addr);
                    self.show_toast(t.copied);
                }
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            field_label(ui, t.primary_name, theme);
            ui.add(
                egui::TextEdit::singleline(&mut self.config.primary_name)
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(14.0);
            // Primary action — filled accent button.
            let save = egui::Button::new(
                egui::RichText::new(t.save).strong().color(Color32::WHITE),
            )
            .min_size(vec2(ui.available_width(), 36.0))
            .rounding(9.0)
            .fill(theme.accent);
            if ui.add(save).clicked() {
                self.config.layout = self.shared_layout.lock().unwrap().clone();
                save_config(&self.config);
                self.show_toast(t.saved_hint);
            }

            if let Some((_, msg)) = &self.toast {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("✓").strong().color(COL_ME).size(13.0));
                    ui.label(egui::RichText::new(msg.clone()).size(12.5).color(COL_ME));
                });
            }
        });
    }

    fn screens_card(&mut self, ui: &mut egui::Ui, t: Tr) {
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            section_header(ui, t.section_screens);
            ui.label(egui::RichText::new(t.screens_hint).weak().size(12.0));
            ui.add_space(4.0);

            let mut layout = self.shared_layout.lock().unwrap();
            let mut dup_idx: Option<usize> = None;
            let mut del_idx: Option<usize> = None;
            for (i, s) in layout.screens.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{}  {}×{}", s.name, s.w, s.h));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(t.del).clicked() {
                            del_idx = Some(i);
                        }
                        if ui.small_button(t.dup).clicked() {
                            dup_idx = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = dup_idx {
                layout.duplicate_screen(i);
            }
            if let Some(i) = del_idx {
                if layout.screens.len() > 1 {
                    layout.screens.remove(i);
                } else {
                    self.toast = Some((Instant::now(), t.keep_one.to_string()));
                }
            }

            ui.add_space(8.0);
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
                    is_local: false,
                });
            }
        });
    }

    fn status_card(&mut self, ui: &mut egui::Ui, t: Tr, _theme: UiTheme) {
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            section_header(ui, t.section_status);
            let peers = self.net.lock().unwrap().peer_count();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(t.peers).weak());
                ui.label(egui::RichText::new(format!("{}", peers)).strong().size(15.0));
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(t.local_name).weak());
                ui.monospace(&self.my_name);
            });
            ui.add_space(8.0);
            // Connection state (primary = serving; secondary = linked to host; idle = not connected).
            let conn_label = match &*self.net.lock().unwrap() {
                Net::Primary { .. } => t.conn_primary,
                Net::Secondary { .. } => t.conn_connected,
                Net::Idle => t.conn_idle,
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(t.conn_status).weak());
                ui.label(egui::RichText::new(conn_label).strong().size(15.0));
            });
            // Live control-plane state (primary only): who has the mouse right now, and —
            // while the cursor is pinned against a shared edge — how many pushes are in.
            if self.config.mode == "primary" {
                let c = self.ctrl.lock().unwrap();
                let line = if let Some(r) = &c.remote {
                    t.ctrl_remote.replace("{}", &r.name)
                } else if c.pins > 0
                    && c.last_pin
                        .map(|t0| t0.elapsed().as_millis() < crate::PIN_WINDOW_MS)
                        .unwrap_or(false)
                {
                    t.ctrl_pushing
                        .replace("{n}", &c.pins.to_string())
                        .replace("{total}", &crate::PIN_THRESHOLD.to_string())
                } else {
                    t.ctrl_local.to_string()
                };
                drop(c);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t.ctrl_status).weak());
                    ui.label(egui::RichText::new(line).strong().size(15.0));
                });
            }
            if self.config.mode == "secondary" {
                let is_idle = matches!(&*self.net.lock().unwrap(), Net::Idle);
                if is_idle {
                    ui.add_space(8.0);
                    if ui.button(t.reconnect_host).clicked() {
                        self.reconnect();
                    }
                }
            }
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(t.hotkey_hint)
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(t.background_hint)
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            if ui.button(t.exit_app).clicked() {
                // Persist the (possibly dragged) layout before quitting on the primary.
                if self.config.mode == "primary" {
                    self.config.layout = self.shared_layout.lock().unwrap().clone();
                    save_config(&self.config);
                }
                std::process::exit(0);
            }
        });
    }
}

/// A macOS-style inset card: subtle fill, hairline border, 12px radius.
fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    egui::Frame::none()
        .fill(ui.visuals().extreme_bg_color)
        .rounding(Rounding::same(12.0))
        .inner_margin(egui::Margin::same(14.0))
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .show(ui, body);
}

/// Section title inside a card.
fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(13.0).strong().color(ui.visuals().strong_text_color()));
    ui.add_space(8.0);
}

/// Small caption above an input field.
fn field_label(ui: &mut egui::Ui, text: &str, _theme: UiTheme) {
    ui.label(egui::RichText::new(text).size(12.0).color(ui.visuals().weak_text_color()));
    ui.add_space(4.0);
}

fn legend_chip(ui: &mut egui::Ui, color: Color32, text: &str, theme: UiTheme) {
    let (_, r) = ui.allocate_space(vec2(11.0, 11.0));
    ui.painter().rect_filled(r, 3.0, color);
    ui.label(egui::RichText::new(text).size(12.0).color(theme.canvas_muted));
}

/// Draw a small mouse glyph (the app icon) in the given color.
fn draw_mouse_icon(p: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let w = rect.width();
    let h = rect.height();
    let body = Rect::from_center_size(c, vec2(w, h));
    // Soft drop shadow.
    p.rect_filled(
        Rect::from_center_size(c + vec2(0.0, 1.0), vec2(w, h)),
        h * 0.5,
        Color32::from_black_alpha(35),
    );
    // Body (vertical pill).
    p.rect_filled(body, h * 0.5, color);
    // Scroll wheel near the top.
    let wheel_w = w * 0.2;
    let wheel_h = h * 0.18;
    let wheel = Rect::from_center_size(
        pos2(c.x, rect.top() + h * 0.28),
        vec2(wheel_w, wheel_h),
    );
    p.rect_filled(wheel, wheel_w * 0.5, Color32::from_white_alpha(200));
}

/// Draw the virtual desktop. Returns `true` when the layout was changed by dragging, so the
/// caller can persist it.
fn draw_layout(
    ui: &mut egui::Ui,
    layout: &mut Layout,
    my_name: &str,
    t: Tr,
    theme: UiTheme,
    canvas_rect: Rect,
) -> bool {
    let mut changed = false;
    // The caller has already reserved the exact rectangle left in the central panel after the
    // header. We just draw into it, using its origin so tiles never creep up and occlude the
    // title/hint/legend.
    let avail = canvas_rect.size();
    if avail.x < 40.0 || avail.y < 40.0 {
        return false;
    }

    let (minx, miny, maxx, maxy) = bounds(layout);
    let vw = (maxx - minx).max(1) as f32;
    let vh = (maxy - miny).max(1) as f32;
    let pad = 56.0;
    let scale = ((avail.x - pad * 2.0) / vw)
        .min((avail.y - pad * 2.0) / vh)
        .max(0.05);
    let offx = canvas_rect.min.x + (avail.x - vw * scale) / 2.0 - minx as f32 * scale;
    let offy = canvas_rect.min.y + (avail.y - vh * scale) / 2.0 - miny as f32 * scale;

    // Bounding box of the primary's own displays, for magnet-snapping remote tiles flush
    // while they are dragged (the cursor can only cross when a remote sits at the edge).
    let lbb = layout.local_bbox();

    for s in layout.screens.iter_mut() {
        let x = offx + s.ox as f32 * scale;
        let y = offy + s.oy as f32 * scale;
        let w = s.w as f32 * scale;
        let h = s.h as f32 * scale;
        let rect = Rect::from_min_size(pos2(x, y), vec2(w, h));

        let is_primary = s.is_local;
        let is_me = s.name == my_name;
        let resp = ui.interact(rect, Id::new(("screen", &s.name)), Sense::drag());
        if resp.dragged() {
            let d = resp.drag_delta();
            s.ox += (d.x / scale) as i32;
            s.oy += (d.y / scale) as i32;
            changed = true;
            // Magnetic snap: pull a remote tile flush against the local bounding box when it
            // comes close, so the shared edge lines up and the cursor can cross. Without this,
            // a tile dragged "almost" flush could silently disable crossing.
            if !s.is_local {
                if let Some((bl, bt, br, bb)) = lbb {
                    const SNAP: f64 = 24.0;
                    let sl = s.ox as f64;
                    let st = s.oy as f64;
                    let sr = sl + s.w as f64;
                    let sb = st + s.h as f64;
                    if (sl - br).abs() <= SNAP {
                        s.ox = br as i32; // flush against the bbox's right edge
                    }
                    if (sr - bl).abs() <= SNAP {
                        s.ox = (bl - s.w as f64) as i32; // flush against the left edge
                    }
                    if (st - bt).abs() <= SNAP {
                        s.oy = bt as i32; // top-aligned with the bbox
                    }
                    if (sb - bb).abs() <= SNAP {
                        s.oy = (bb - s.h as f64) as i32; // bottom-aligned
                    }
                }
            }
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
            COL_CLIENT
        };
        let stroke = if resp.hovered() || resp.dragged() {
            Color32::from_white_alpha(235)
        } else {
            Color32::from_white_alpha(120)
        };

        // Soft drop shadow, then the tile.
        ui.painter().rect_filled(
            rect.translate(vec2(0.0, 6.0)),
            12.0,
            Color32::from_black_alpha(70),
        );
        ui.painter().rect_filled(rect, 12.0, fill);
        // Top sheen for a bit of depth.
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, vec2(rect.width(), rect.height().min(14.0))),
            12.0,
            Color32::from_white_alpha(28),
        );
        ui.painter().rect_stroke(rect, 12.0, (1.5, stroke));

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
                FontId::proportional(15.5),
                Color32::WHITE,
            );
            if h > 76.0 {
                ui.painter().text(
                    pos2(rect.center().x, cy + 12.0),
                    Align2::CENTER_CENTER,
                    &format!("{}×{}", s.w, s.h),
                    FontId::proportional(12.0),
                    Color32::from_white_alpha(210),
                );
            }
        }
    }

    // Bottom-center hint on the canvas.
    ui.painter().text(
        pos2(canvas_rect.min.x + avail.x / 2.0, canvas_rect.max.y - 16.0),
        Align2::CENTER_CENTER,
        t.layout_tip,
        FontId::proportional(12.0),
        theme.canvas_muted,
    );
    changed
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
