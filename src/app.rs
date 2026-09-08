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
use crate::discovery::DiscoveredList;
use crate::i18n::{tr, Lang, Tr};
use crate::layout::Layout;
use crate::network::{connect_client, Net};
use crate::protocol::Message;
use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Id, Rect, Sense};
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
    /// Canvas dot-grid colour — barely-there texture so the virtual desktop does not read as
    /// one flat slab of colour.
    grid: Color32,
    /// Shadow cast by a screen tile, plus the halo drawn around it while hovering/dragging.
    shadow: Color32,
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
                grid: Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 96),
            }
        } else {
            UiTheme {
                canvas_bg: Color32::from_rgb(243, 243, 248), // systemGray6
                canvas_text: Color32::from_rgb(60, 60, 67),  // label
                canvas_muted: Color32::from_rgb(142, 142, 147),
                accent: Color32::from_rgb(0, 122, 255),
                hairline: Color32::from_rgba_unmultiplied(0, 0, 0, 12),
                grid: Color32::from_rgba_unmultiplied(0, 0, 0, 26),
                shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 40),
            }
        }
    }
}

// ---- Canvas drawing helpers -----------------------------------------------------------
//
// egui has no gradient or shadow primitives, so a few small painters do the work. They are
// cheap (a couple of dozen rects per tile) and keep the canvas looking like a designed
// surface rather than flat coloured boxes.

/// Linear blend between two colours.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t) as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// Fill a rounded rect with a vertical gradient.
///
/// Drawn as horizontal bands with per-corner radii (top band rounds the top corners, bottom
/// band rounds the bottom ones, middle bands stay square) — that is what makes the result a
/// *rounded* gradient rect instead of a rectangle with gradient stripes over it.
fn fill_gradient(painter: &egui::Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    let steps = rect.height().max(8.0) / 4.0; // one band per ~4 px
    let steps = steps.clamp(4.0, 40.0) as i32;
    let band = rect.height() / steps as f32;
    for i in 0..steps {
        let t = i as f32 / (steps - 1).max(1) as f32;
        let r = egui::CornerRadius {
            nw: if i == 0 { radius as u8 } else { 0 },
            ne: if i == 0 { radius as u8 } else { 0 },
            sw: if i == steps - 1 { radius as u8 } else { 0 },
            se: if i == steps - 1 { radius as u8 } else { 0 },
        };
        painter.rect_filled(
            Rect::from_min_size(
                pos2(rect.min.x, rect.min.y + i as f32 * band),
                vec2(rect.width(), band + 0.8), // overlap kills seams between bands
            ),
            r,
            mix(top, bottom, t),
        );
    }
}

/// Layered soft shadow: a few progressively wider, fainter rounded rects pushed downward.
/// Reads far closer to a real drop shadow than a single hard offset rect.
fn soft_shadow(painter: &egui::Painter, rect: Rect, radius: f32, color: Color32, lift: f32) {
    let layers = 5;
    for i in 1..=layers {
        let f = i as f32 / layers as f32;
        let spread = 2.0 + f * 10.0 * lift;
        let alpha = (color.a() as f32 * (1.0 - f) * 0.34) as u8;
        if alpha == 0 {
            continue;
        }
        painter.rect_filled(
            rect.expand(spread).translate(vec2(0.0, f * 7.0 * lift)),
            egui::CornerRadius::same((radius + spread) as u8),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
        );
    }
}

/// Dot grid over the canvas — texture without visual noise.
fn dot_grid(painter: &egui::Painter, rect: Rect, color: Color32) {
    const STEP: f32 = 26.0;
    let mut y = rect.min.y + STEP;
    while y < rect.max.y {
        let mut x = rect.min.x + STEP;
        while x < rect.max.x {
            painter.circle_filled(pos2(x, y), 1.1, color);
            x += STEP;
        }
        y += STEP;
    }
}

/// Tile gradient pair for a screen role: (top, bottom).
fn tile_colors(is_primary: bool, is_me: bool) -> (Color32, Color32) {
    if is_primary {
        (Color32::from_rgb(90, 165, 255), Color32::from_rgb(0, 92, 214))
    } else if is_me {
        (Color32::from_rgb(96, 219, 130), Color32::from_rgb(24, 160, 74))
    } else {
        (Color32::from_rgb(158, 158, 167), Color32::from_rgb(88, 88, 98))
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
            .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(embedded.to_vec())));
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
                    .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
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
    let dark = style.visuals.dark_mode;

    // Rhythm: generous, consistent spacing is most of what makes a UI feel designed.
    style.spacing.item_spacing = vec2(10.0, 11.0);
    style.spacing.button_padding = vec2(15.0, 8.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.indent = 16.0;
    style.spacing.window_margin = egui::Margin::same(0);
    // Text never gets cramped inside a field.
    style.spacing.text_edit_width = 220.0;
    style.spacing.combo_width = 220.0;
    style.spacing.scroll.bar_width = 8.0;

    // Uniform control rounding — the macOS squircle look.
    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
        &mut style.visuals.widgets.noninteractive,
    ] {
        w.corner_radius = egui::CornerRadius::same(10);
        // A hairline on every control keeps the panel from looking like a wall of flat fills.
        w.bg_stroke = egui::Stroke::new(
            1.0_f32,
            if dark {
                Color32::from_white_alpha(26)
            } else {
                Color32::from_black_alpha(20)
            },
        );
    }
    style.visuals.widgets.hovered.expansion = 0.0;
    style.visuals.widgets.active.expansion = 0.0;

    // Slightly tinted panel background so the sidebar reads as a distinct surface from the
    // central canvas instead of merging into it.
    style.visuals.panel_fill = if dark {
        Color32::from_rgb(24, 24, 29)
    } else {
        Color32::from_rgb(249, 249, 252)
    };
    style.visuals.window_fill = style.visuals.panel_fill;
    style.visuals.window_stroke = egui::Stroke::NONE;

    // Softer scrollbar that fades rather than shouts.
    style.visuals.handle_shape = egui::style::HandleShape::Circle;

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
    /// Primaries seen on the LAN via UDP discovery (secondary only). The listener appends to it;
    /// the "discovered devices" card reads it so the user can connect with one click.
    pub discovered: DiscoveredList,
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
        discovered: DiscoveredList,
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
            discovered,
        }
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((Instant::now(), msg.into()));
    }

    /// (Re)connect to the primary at `addr` from the running app. Tearing down to `Idle` first
    /// lets the old reader/writer threads stop, then we open a fresh connection and send Hello.
    /// No app restart required. Returns nothing; connection state is reflected via `self.net`.
    fn connect_to(&mut self, addr: String) {
        let t = tr(self.lang);
        let addr = addr.trim().to_string();
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

    /// (Re)connect using the configured `server_addr`. Used by the "Connect" button and the
    /// startup-error "Retry" banner.
    fn reconnect(&mut self) {
        let addr = self.config.server_addr.trim().to_string();
        self.connect_to(addr);
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

        // Hand the context to the tray module (Windows) so its menu can restore this window.
        #[cfg(target_os = "windows")]
        crate::tray::register_ctx(ctx);

        // Keep running when the window is closed: sharing (input capture/injection, clipboard,
        // network) lives on background threads that don't need the window. Cancel the close and
        // minimise instead of exiting — the tray menu (Windows) or the "Quit" button exits for
        // real. Without this the process died with the window and the user had to keep the
        // window open for sharing to work at all.
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

        // Auto-discovery may have linked us in the background (the listener thread flips `net`
        // to `Secondary`). Clear any stale startup-error banner so the UI reflects the live state.
        if matches!(&*self.net.lock().unwrap(), Net::Secondary { .. }) && self.startup_error.is_some() {
            self.startup_error = None;
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
                    // Live connection status: a coloured dot + short label, so the user sees at
                    // a glance whether sharing is live without opening the status card.
                    let (dot_color, status_text) = match &*self.net.lock().unwrap() {
                        Net::Primary { .. } => (COL_ME, t.conn_primary),
                        Net::Secondary { .. } => (COL_ME, t.conn_connected),
                        Net::Idle => (Color32::from_rgb(255, 159, 10), t.conn_idle),
                    };
                    ui.label(egui::RichText::new(status_text).weak().size(12.5));
                    ui.label(egui::RichText::new("●").color(dot_color).size(14.0));
                    ui.add_space(14.0);
                    let pill = egui::Button::new(
                        egui::RichText::new(self.lang.toggle_label()).size(12.5),
                    )
                    .corner_radius(8)
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
                egui::Frame::NONE
                    .fill(Color32::from_rgb(255, 235, 236))
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .corner_radius(10)
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
                        if self.config.mode == "secondary" {
                            self.discovered_card(ui, t, theme);
                        }
                        self.screens_card(ui, t);
                        self.status_card(ui, t, theme);
                    });
            });

        // ---- Central canvas: the virtual desktop ----
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme.canvas_bg))
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
                        scale: 1.0,
                    });
                }
                // Live cursor position from the control plane, drawn on the canvas: while you
                // move the mouse this dot must track it. A dot that doesn't move or sits in
                // the wrong place means the reported coordinates don't match the layout —
                // visible at a glance instead of guesswork.
                let cur = self.ctrl.lock().unwrap().last_real;
                let layout_changed = draw_layout(
                    ui,
                    &mut layout,
                    &self.config.name,
                    t,
                    theme,
                    canvas_rect,
                    cur,
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
            .corner_radius(9)
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
                    let phys = s.physical_size();
                    let size_label = if phys != (s.w, s.h) {
                        // HiDPI panel: logical (layout) size @ scale, plus the real pixel size.
                        format!("{}×{} @{:.2}x ({}×{})", s.w, s.h, s.scale, phys.0, phys.1)
                    } else {
                        format!("{}×{}", s.w, s.h)
                    };
                    ui.monospace(format!("{}  {}", s.name, size_label));
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
                    scale: 1.0,
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
            if self.config.mode == "primary" {
                // Point primary users at the diagnostic log: if crossing misbehaves, this file
                // contains the exact decisions (pins, hand-offs, samples) needed to diagnose it.
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{} {}",
                        t.diag_hint,
                        crate::diag::log_path().display()
                    ))
                    .size(11.0)
                    .color(ui.visuals().weak_text_color()),
                );
            }
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

    /// "Discovered on LAN" card (secondary only). Lists primaries heard via the UDP beacon and
    /// lets the user connect with one click — this is the visible half of auto-discovery.
    fn discovered_card(&mut self, ui: &mut egui::Ui, t: Tr, _theme: UiTheme) {
        let list = self.discovered.lock().unwrap().clone();
        card(ui, |ui| {
            ui.set_width(ui.available_width());
            section_header(ui, t.section_discovered);
            if list.is_empty() {
                ui.label(egui::RichText::new(t.discovered_empty).weak().size(12.0));
            } else {
                for d in &list {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{}  ·  {}", d.name, d.addr()))
                                .strong()
                                .size(13.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(t.discovered_connect).clicked() {
                                self.config.server_addr = d.addr();
                                self.connect_to(d.addr());
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
            }
        });
    }
}

/// A macOS-style inset card: subtle fill, hairline border, 12px radius.
fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    egui::Frame::NONE
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(12)
        .inner_margin(egui::Margin::same(14))
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
    cur: (f64, f64),
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

    // Canvas texture: a faint dot grid so the empty area reads as a surface, not a void.
    dot_grid(ui.painter(), canvas_rect, theme.grid);

    // The tile currently being dragged, painted last so it floats above the others.
    let mut dragged: Option<(Rect, bool, bool, String, (u32, u32), f32)> = None;

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
        let hover = resp.hovered() || resp.dragged();

        let (top, bottom) = tile_colors(is_primary, is_me);
        // A dragged tile is deferred to the end of the loop so it floats above the others
        // instead of sliding underneath them.
        if resp.dragged() {
            dragged = Some((
                rect,
                is_primary,
                is_me,
                s.name.clone(),
                s.physical_size(),
                s.scale,
            ));
            continue;
        }
        soft_shadow(
            ui.painter(),
            rect,
            16.0,
            theme.shadow,
            if hover { 1.3 } else { 1.0 },
        );
        paint_tile(
            ui.painter(),
            rect,
            &s.name,
            s.physical_size(),
            s.scale,
            is_primary,
            is_me,
            hover,
            theme,
            top,
            bottom,
        );
    }

    // The actively dragged tile, painted on top of everything.
    if let Some((rect, is_primary, is_me, name, phys, sc)) = dragged {
        let (top, bottom) = tile_colors(is_primary, is_me);
        soft_shadow(ui.painter(), rect, 16.0, theme.shadow, 2.0);
        paint_tile(
            ui.painter(),
            rect,
            &name,
            phys,
            sc,
            is_primary,
            is_me,
            true,
            theme,
            top,
            bottom,
        );
    }

    // Shared edges: where this machine's displays meet a secondary's, the cursor can cross.
    // Making them visible turns "why can't I cross?" into something you can see at a glance —
    // a missing or misaligned shared edge is the usual answer.
    paint_shared_edges(ui.painter(), layout, offx, offy, scale, theme);

    // Live cursor dot: the control plane's idea of where the real cursor is. While you move
    // the mouse on this machine the dot must track it 1:1 — if it doesn't (or sits elsewhere)
    // the reported coordinates don't match the layout, which is the #1 crossing killer and
    // now visible at a glance.
    let cx = offx + cur.0 as f32 * scale;
    let cy = offy + cur.1 as f32 * scale;
    let in_view = cx >= canvas_rect.min.x - 8.0
        && cx <= canvas_rect.max.x + 8.0
        && cy >= canvas_rect.min.y - 8.0
        && cy <= canvas_rect.max.y + 8.0;
    if in_view {
        let p = pos2(cx, cy);
        ui.painter()
            .circle_filled(p, 7.0, Color32::from_rgba_unmultiplied(255, 170, 0, 90));
        ui.painter().circle_filled(p, 3.5, Color32::from_rgb(255, 170, 0));
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

/// Paint one screen tile: a gradient "monitor" with an inset glass area, a specular top
/// edge, and the machine name / resolution inside.
#[allow(clippy::too_many_arguments)]
fn paint_tile(
    painter: &egui::Painter,
    rect: Rect,
    name: &str,
    phys: (u32, u32),
    scale: f32,
    is_primary: bool,
    is_me: bool,
    hover: bool,
    theme: UiTheme,
    top: Color32,
    bottom: Color32,
) {
    const R: f32 = 16.0;
    fill_gradient(painter, rect, R, top, bottom);

    // Inset "glass" panel — the part that reads as the actual display.
    let inset = rect.shrink(9.0);
    if inset.width() > 4.0 && inset.height() > 4.0 {
        painter.rect_filled(
            inset,
            egui::CornerRadius::same(9),
            Color32::from_white_alpha(if hover { 30 } else { 18 }),
        );
    }

    // Specular highlight along the top edge.
    let sheen = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height().min(3.0)));
    painter.rect_filled(
        sheen,
        egui::CornerRadius { nw: R as u8, ne: R as u8, sw: 0, se: 0 },
        Color32::from_white_alpha(55),
    );

    // Border: subtle normally, a bright accent halo while hovered/dragged.
    let stroke = if hover {
        (2.5, Color32::from_white_alpha(235))
    } else {
        (1.0, Color32::from_white_alpha(70))
    };
    painter.rect_stroke(rect, egui::CornerRadius::same(R as u8), stroke, egui::StrokeKind::Inside);
    if hover {
        painter.rect_stroke(
            rect.expand(4.0),
            egui::CornerRadius::same((R + 4.0) as u8),
            (2.0, Color32::from_rgba_unmultiplied(theme.accent.r(), theme.accent.g(), theme.accent.b(), 150)),
            egui::StrokeKind::Outside,
        );
    }

    // Label: name (with a star on the primary's own displays) + resolution.
    if rect.width() > 56.0 && rect.height() > 40.0 {
        let title = if is_primary {
            format!("★ {name}")
        } else {
            name.to_string()
        };
        let cy = rect.center().y;
        let two_line = rect.height() > 76.0;
        painter.text(
            pos2(rect.center().x, if two_line { cy - 11.0 } else { cy }),
            Align2::CENTER_CENTER,
            title,
            FontId::proportional(15.5),
            Color32::WHITE,
        );
        if two_line {
            painter.text(
                pos2(rect.center().x, cy + 11.0),
                Align2::CENTER_CENTER,
                if phys != (rect.width() as u32, rect.height() as u32) && scale != 1.0 {
                    format!("{}×{}  @{}x", phys.0, phys.1, scale)
                } else {
                    format!("{}×{}", phys.0, phys.1)
                },
                FontId::proportional(12.0),
                Color32::from_white_alpha(215),
            );
        }
        if is_me && rect.height() > 110.0 {
            painter.text(
                pos2(rect.center().x, rect.max.y - 16.0),
                Align2::CENTER_CENTER,
                "本机",
                FontId::proportional(11.0),
                Color32::from_white_alpha(190),
            );
        }
    }
}

/// Highlight every edge where one of this machine's displays touches a secondary's — those
/// are the only places the cursor can cross.
fn paint_shared_edges(
    painter: &egui::Painter,
    layout: &Layout,
    offx: f32,
    offy: f32,
    scale: f32,
    theme: UiTheme,
) {
    let (ar, ag, ab) = (theme.accent.r(), theme.accent.g(), theme.accent.b());
    for a in layout.screens.iter() {
        if !a.is_local {
            continue;
        }
        let al = a.ox as f64;
        let at = a.oy as f64;
        let ar_ = al + a.w as f64;
        let ab_ = at + a.h as f64;
        for b in layout.screens.iter() {
            if b.is_local {
                continue;
            }
            let bl = b.ox as f64;
            let bt = b.oy as f64;
            let br = bl + b.w as f64;
            let bb = bt + b.h as f64;
            // Vertical shared edge: a's right side flush with b's left (or mirrored).
            let (ex, y0, y1) = if (ar_ - bl).abs() <= 1.0 {
                (ar_, at.max(bt), ab_.min(bb))
            } else if (br - al).abs() <= 1.0 {
                (al, at.max(bt), ab_.min(bb))
            } else {
                continue;
            };
            if y1 <= y0 + 1.0 {
                continue;
            }
            let x = offx + ex as f32 * scale;
            let p0 = pos2(x, offy + y0 as f32 * scale);
            let p1 = pos2(x, offy + y1 as f32 * scale);
            painter.line_segment([p0, p1], (7.0, Color32::from_rgba_unmultiplied(ar, ag, ab, 55)));
            painter.line_segment([p0, p1], (2.5, Color32::from_rgb(ar, ag, ab)));
        }
    }
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
