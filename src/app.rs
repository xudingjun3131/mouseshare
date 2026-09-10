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
use crate::control::GrabCtx;
use crate::discovery::DiscoveredList;
use crate::i18n::{tr, Lang, Tr};
use crate::layout::Layout;
use crate::network::{connect_client, Net};
use crate::protocol::Message;
use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Id, Rect, Sense};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Vertical clearance for the macOS traffic-light cluster (close / minimise / zoom).
///
/// The window runs with `fullsize_content_view(true)` + `titlebar_shown(false)`, so the traffic
/// lights float *on top of* the egui canvas rather than living in a reserved strip. They occupy
/// roughly y ∈ [8, 24] and x ∈ [12, 76]. Anything drawn in the sidebar's top-left corner without
/// this clearance ends up underneath them — unreadable and, worse, unclickable, because the
/// buttons take the hit. 38pt puts content comfortably below the cluster on every macOS build
/// we've seen. Windows and Linux draw a normal title bar, so they need no clearance at all.
#[cfg(target_os = "macos")]
const TITLEBAR_CLEARANCE: i8 = 38;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_CLEARANCE: i8 = 14;

/// Notifications queued by background threads (file transfers) for the GUI to toast.
/// The GUI can only be touched from its own thread, so workers push strings here instead.
static NOTIFICATIONS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Queue a transient message for the GUI. Safe to call from any thread.
pub fn notify(msg: impl Into<String>) {
    let q = NOTIFICATIONS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut v) = q.lock() {
        // Never let a stalled GUI accumulate messages unbounded.
        if v.len() < 8 {
            v.push(msg.into());
        }
    }
}

/// Take everything queued since the last call (called once per frame).
fn take_notifications() -> Vec<String> {
    let q = NOTIFICATIONS.get_or_init(|| Mutex::new(Vec::new()));
    std::mem::take(&mut *q.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The sidebar page currently shown in the main area.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Connection,
    Layout,
    Status,
    Discovered,
}

/// Theme-derived palette.
///
/// The palette follows macOS system colours (Apple's "systemBlue", "label"/"secondaryLabel",
/// "windowBackground"/"sidebarBackground", "secondarySystemFill" …) so the chrome reads as a native
/// app in both light and dark appearances. The surfaces are deliberately *layered* — sidebar sits
/// behind the content, cards sit on top of it — which is what makes it read as macOS rather than
/// one flat slab.
#[derive(Clone, Copy)]
struct UiTheme {
    /// Main content area behind the cards.
    window_bg: Color32,
    /// Sidebar / navigation material.
    sidebar_bg: Color32,
    toolbar_bg: Color32,
    /// Card surface (white in light mode, secondarySystemFill in dark).
    card_bg: Color32,
    card_stroke: Color32,
    /// Text-field background and the muted fill used by stat tiles.
    field_bg: Color32,
    fill_bg: Color32,
    /// label / secondaryLabel / tertiaryLabel.
    text: Color32,
    muted: Color32,
    faint: Color32,
    accent: Color32,
    accent_tint: Color32,
    accent_tint_strong: Color32,
    hairline: Color32,
    divider: Color32,
    /// Canvas dot-grid colour — barely-there texture so the virtual desktop does not read as
    /// one flat slab of colour.
    grid: Color32,
    /// Shadow cast by a screen tile, plus the halo drawn around it while hovering/dragging.
    shadow: Color32,
    /// Semantic status colours (systemGreen / systemOrange / systemRed) + their tints.
    green: Color32,
    green_tint: Color32,
    orange: Color32,
    orange_tint: Color32,
    red: Color32,
    red_tint: Color32,
    /// Sidebar row hover / selected backgrounds.
    nav_hover: Color32,
    nav_active: Color32,
    /// Secondary ("bordered") button surfaces — flat fill + hover lift.
    btn_bg: Color32,
    btn_hover: Color32,
}

impl UiTheme {
    fn from_ctx(ctx: &egui::Context) -> Self {
        let dark = ctx.style().visuals.dark_mode;
        if dark {
            UiTheme {
                window_bg: Color32::from_rgb(30, 30, 32),       // windowBackground
                sidebar_bg: Color32::from_rgb(38, 38, 40),      // sidebarBackground
                toolbar_bg: Color32::from_rgb(44, 44, 46),
                card_bg: Color32::from_rgb(44, 44, 46),         // secondarySystemFill
                card_stroke: Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                field_bg: Color32::from_rgb(28, 28, 30),
                fill_bg: Color32::from_rgb(58, 58, 60),         // tertiarySystemFill
                text: Color32::from_rgb(245, 245, 247),         // label
                muted: Color32::from_rgb(152, 152, 157),        // secondaryLabel
                faint: Color32::from_rgb(99, 99, 102),          // tertiaryLabel
                accent: Color32::from_rgb(10, 132, 255),        // systemBlue (dark)
                accent_tint: Color32::from_rgba_unmultiplied(10, 132, 255, 46),
                accent_tint_strong: Color32::from_rgba_unmultiplied(10, 132, 255, 72),
                hairline: Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                divider: Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                grid: Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 140),
                green: Color32::from_rgb(48, 209, 88),
                green_tint: Color32::from_rgba_unmultiplied(48, 209, 88, 46),
                orange: Color32::from_rgb(255, 159, 10),
                orange_tint: Color32::from_rgba_unmultiplied(255, 159, 10, 46),
                red: Color32::from_rgb(255, 69, 58),
                red_tint: Color32::from_rgba_unmultiplied(255, 69, 58, 46),
                nav_hover: Color32::from_rgba_unmultiplied(255, 255, 255, 15),
                nav_active: Color32::from_rgba_unmultiplied(255, 255, 255, 31),
                btn_bg: Color32::from_rgb(58, 58, 60),
                btn_hover: Color32::from_rgb(72, 72, 75),
            }
        } else {
            UiTheme {
                window_bg: Color32::from_rgb(246, 246, 248),    // windowBackground
                sidebar_bg: Color32::from_rgb(236, 236, 239),   // sidebarBackground
                toolbar_bg: Color32::from_rgb(243, 243, 245),
                card_bg: Color32::from_rgb(255, 255, 255),      // white card on gray
                card_stroke: Color32::from_rgba_unmultiplied(0, 0, 0, 20),
                field_bg: Color32::from_rgb(255, 255, 255),
                fill_bg: Color32::from_rgb(245, 245, 247),      // systemGray6
                text: Color32::from_rgb(29, 29, 31),            // label
                muted: Color32::from_rgb(134, 134, 139),        // secondaryLabel
                faint: Color32::from_rgb(174, 174, 178),        // tertiaryLabel
                accent: Color32::from_rgb(0, 122, 255),         // systemBlue (light)
                accent_tint: Color32::from_rgba_unmultiplied(0, 122, 255, 26),
                accent_tint_strong: Color32::from_rgba_unmultiplied(0, 122, 255, 46),
                hairline: Color32::from_rgba_unmultiplied(0, 0, 0, 20),
                divider: Color32::from_rgba_unmultiplied(0, 0, 0, 20),
                grid: Color32::from_rgba_unmultiplied(0, 0, 0, 26),
                shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 31),
                green: Color32::from_rgb(52, 199, 89),
                green_tint: Color32::from_rgba_unmultiplied(52, 199, 89, 31),
                orange: Color32::from_rgb(255, 149, 0),
                orange_tint: Color32::from_rgba_unmultiplied(255, 149, 0, 36),
                red: Color32::from_rgb(255, 59, 48),
                red_tint: Color32::from_rgba_unmultiplied(255, 59, 48, 26),
                nav_hover: Color32::from_rgba_unmultiplied(0, 0, 0, 10),
                nav_active: Color32::from_rgba_unmultiplied(0, 0, 0, 20),
                btn_bg: Color32::from_rgb(234, 234, 238),
                btn_hover: Color32::from_rgb(223, 223, 228),
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

/// Tile gradient pair for a screen role: (top, bottom). macOS systemBlue / systemGreen / gray.
fn tile_colors(is_primary: bool, is_me: bool) -> (Color32, Color32) {
    if is_primary {
        // Soft systemBlue — light enough to read, not the heavy saturated blue the previous
        // palette produced.
        (Color32::from_rgb(132, 190, 248), Color32::from_rgb(64, 138, 230))
    } else if is_me {
        // Soft systemGreen — this machine.
        (Color32::from_rgb(132, 220, 164), Color32::from_rgb(70, 182, 110))
    } else {
        // Neutral gray client.
        (Color32::from_rgb(178, 178, 190), Color32::from_rgb(132, 132, 146))
    }
}

/// Install the typefaces.
///
/// On macOS we lead each family with **San Francisco** (`/System/Library/Fonts/SFNS.ttf`) so Latin
/// text renders in the exact same UI font as the rest of the OS, then keep the bundled Noto Sans SC
/// *behind* it as the CJK fallback. ab_glyph picks the first family that owns a glyph, so Latin uses
/// SF and Chinese/Japanese/Korean fall through to Noto — one consistent, native-looking face per
/// language. On Windows/Linux (no SF) we keep Noto Sans SC first so CJK always works, and Noto's own
/// Latin glyphs keep the two languages visually matched.
///
/// The embedded Noto OTF is the only thing guaranteed across every machine (CI, Windows without
/// East-Asian packs, headless Linux), so it is always registered; the system scan below only adds
/// fallbacks for machines where the embedded file is somehow missing.
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // macOS system UI font (San Francisco). Read once; reused for every family.
    #[cfg(target_os = "macos")]
    let sf_bytes: Option<Vec<u8>> = std::fs::read("/System/Library/Fonts/SFNS.ttf").ok();
    #[cfg(not(target_os = "macos"))]
    let sf_bytes: Option<Vec<u8>> = None;

    // Bundled Noto Sans SC — the guaranteed CJK face.
    let embedded: &[u8] = include_bytes!("../resources/NotoSansSC-Regular.otf");
    let has_cjk = !embedded.is_empty();
    if has_cjk {
        fonts
            .font_data
            .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(embedded.to_vec())));
        log::info!("using bundled Noto Sans SC ({} KB)", embedded.len() / 1024);
    }

    if let Some(b) = &sf_bytes {
        fonts
            .font_data
            .insert("sf".into(), std::sync::Arc::new(egui::FontData::from_owned(b.clone())));
    }

    // Build each family: system UI font (macOS) first, then CJK fallback, then any discovered
    // system CJK as a last resort.
    let mut cjk_fallback_paths: &[&str] = &[];
    if !has_cjk {
        cjk_fallback_paths = &[
            // macOS
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/System/Library/Fonts/PingFang.ttc",
            // Windows
            "C:/Windows/Fonts/msyh.ttf",
            "C:/Windows/Fonts/simhei.ttf",
            // Linux
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttf",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        ];
    }

    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(fam).or_default();
        if sf_bytes.is_some() && !list.iter().any(|f| f == "sf") {
            list.insert(0, "sf".into());
        }
        if has_cjk && !list.iter().any(|f| f == "cjk") {
            list.push("cjk".into());
        }
        if !has_cjk {
            for p in cjk_fallback_paths {
                if let Ok(bytes) = std::fs::read(p) {
                    fonts.font_data.insert(
                        "cjk".into(),
                        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                    );
                    if !list.iter().any(|f| f == "cjk") {
                        list.push("cjk".into());
                    }
                    break;
                }
            }
        }
    }

    ctx.set_fonts(fonts);
}

/// Global look & feel: macOS-flavored metrics — squircle control rounding, roomy spacing, soft
/// scrollbars. Colors stay theme-driven; the canvas derives its own palette in `UiTheme`.
pub fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let dark = style.visuals.dark_mode;

    // Rhythm: generous, consistent spacing is most of what makes a UI feel designed.
    style.spacing.item_spacing = vec2(12.0, 14.0);
    style.spacing.button_padding = vec2(18.0, 9.0);
    style.spacing.menu_margin = egui::Margin::same(10);
    style.spacing.indent = 20.0;
    style.spacing.window_margin = egui::Margin::same(0);
    // Text never gets cramped inside a field.
    style.spacing.text_edit_width = 260.0;
    style.spacing.combo_width = 260.0;
    style.spacing.scroll.bar_width = 10.0;

    // ---- Typography: a real macOS type scale ----
    // egui's built-in ramp (Small 9 / Body 12.5 / Button 12.5 / Heading 18) is cramped and isn't
    // a coherent scale. macOS uses 11pt small, 13pt body/label/button, 15pt section title and
    // 17pt window title. Everything drawn *without* an explicit `.size()` — buttons, labels,
    // panels, i.e. most of the chrome — picks these up, so this fixes default typography in one
    // place instead of hunting through every widget.
    style.text_styles = [
        (egui::TextStyle::Small, egui::FontId::new(11.0, egui::FontFamily::Proportional)),
        (egui::TextStyle::Body, egui::FontId::new(13.0, egui::FontFamily::Proportional)),
        (egui::TextStyle::Button, egui::FontId::new(13.0, egui::FontFamily::Proportional)),
        (egui::TextStyle::Heading, egui::FontId::new(17.0, egui::FontFamily::Proportional)),
        (egui::TextStyle::Monospace, egui::FontId::new(12.5, egui::FontFamily::Monospace)),
    ]
    .into_iter()
    .collect();

    // Uniform control rounding — the macOS squircle look. Text fields and buttons use 7pt,
    // the tighter radius Apple uses for inline controls (vs. 10–12pt for cards).
    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
        &mut style.visuals.widgets.noninteractive,
    ] {
        w.corner_radius = egui::CornerRadius::same(7);
        // A hairline on every control keeps the panel from looking like a wall of flat fills.
        w.bg_stroke = egui::Stroke::new(
            1.0_f32,
            if dark {
                Color32::from_white_alpha(52)
            } else {
                Color32::from_black_alpha(51)
            },
        );
    }
    style.visuals.widgets.hovered.expansion = 0.0;
    style.visuals.widgets.active.expansion = 0.0;
    // Text fields: white-on-window in light mode (cards are white too, so the border is what
    // defines the field), one step darker than the card in dark mode.
    style.visuals.widgets.inactive.bg_fill = if dark {
        Color32::from_rgb(28, 28, 30)
    } else {
        Color32::from_rgb(255, 255, 255)
    };
    style.visuals.widgets.hovered.bg_fill = style.visuals.widgets.inactive.bg_fill;
    style.visuals.widgets.active.bg_fill = style.visuals.widgets.inactive.bg_fill;

    let theme = UiTheme::from_ctx(ctx);
    // Window + panel share the content material so egui's popups and scroll areas match.
    style.visuals.window_fill = theme.window_bg;
    style.visuals.panel_fill = theme.window_bg;
    style.visuals.window_stroke = egui::Stroke::NONE;
    style.visuals.override_text_color = Some(theme.text);

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
    /// Which sidebar page the main area is showing.
    pub page: Page,
    /// Transient toast message with the moment it was shown (auto-hides after 3 s).
    pub toast: Option<(Instant, String)>,
    /// Set when networking failed at startup (port busy / primary unreachable). Shown as a
    /// banner instead of letting the app exit silently with no window at all.
    pub startup_error: Option<String>,
    /// Incoming channel used to (re)establish a secondary connection from the GUI.
    pub inc_tx: Sender<(String, Message)>,
    /// Throttle timestamp for the primary's periodic layout push to secondaries.
    pub last_layout_push: Option<Instant>,
    /// Cached tail of the diagnostic log, for the status page's activity list.
    pub activity_cache: Vec<(String, String, Color32)>,
    /// When `activity_cache` was last refreshed — the log is re-read at most every 2 s,
    /// otherwise every frame would hit the disk 60×/second for no visible gain.
    pub activity_poll: Option<Instant>,
    /// The capture thread's control-plane state (who has the mouse, edge-push progress).
    /// Shared read-only here so the status card can show live hand-off state.
    pub ctrl: Arc<Mutex<crate::Ctrl>>,
    /// Primaries seen on the LAN via UDP discovery (secondary only). The listener appends to it;
    /// the "discovered devices" card reads it so the user can connect with one click.
    pub discovered: DiscoveredList,
    /// Control-plane context kept so the "re-check" button can restart the capture thread after
    /// the user grants the missing permission.
    pub grab_ctx: Arc<GrabCtx>,
    /// Set by the capture thread when it cannot create the event tap (missing permission). Polled
    /// each frame; drives the permission guidance dialog.
    pub capture_failed: Arc<AtomicBool>,
    /// Once the user dismisses the permission dialog we stop nagging until the next app start
    /// (the capture has already failed and won't retry on its own).
    pub perm_dismissed: bool,
    /// Guards the native permission prompt so we only trigger it once per failure (not every
    /// frame while the dialog is up). Reset when the user clicks "re-check".
    pub perm_prompt_sent: bool,
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
        grab_ctx: Arc<GrabCtx>,
        capture_failed: Arc<AtomicBool>,
    ) -> Self {
        let lang = Lang::from_code(&config.lang);
        Self {
            config,
            shared_layout,
            net,
            my_name,
            lang,
            page: Page::Connection,
            toast: None,
            startup_error,
            inc_tx,
            last_layout_push: None,
            activity_cache: Vec::new(),
            activity_poll: None,
            ctrl,
            discovered,
            grab_ctx,
            capture_failed,
            perm_dismissed: false,
            perm_prompt_sent: false,
        }
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((Instant::now(), msg.into()));
    }

    /// Drain notifications queued from background threads (file transfers, transfer errors).
    /// Background threads cannot touch the GUI, so they push strings here instead.
    fn drain_notifications(&mut self) {
        for msg in take_notifications() {
            self.show_toast(msg);
        }
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
                    scale: crate::input::local_scale(),
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

    /// Render the "missing input permission" guidance dialog. Shown when the capture thread flags
    /// that it could not create the event tap (macOS primary). Handles its own buttons: open the
    /// two System Settings panes, re-run the capture, or dismiss until next launch.
    fn show_permission_dialog(&mut self, ctx: &egui::Context, t: Tr, theme: UiTheme) {
        if !self.capture_failed.load(Ordering::SeqCst) || self.perm_dismissed {
            return;
        }

        // Trigger the native macOS permission prompts exactly once per failure. Done here (on the
        // UI/main thread) rather than in the capture thread, which is where macOS most reliably
        // presents the "allow accessibility / input monitoring" dialog.
        if !self.perm_prompt_sent {
            self.perm_prompt_sent = true;
            crate::capture::trigger_permission_prompts();
        }

        let mut recheck = false;
        let mut dismiss = false;
        let mut open_input = false;
        let mut open_accessibility = false;

        egui::Window::new("perm_dialog")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .order(egui::Order::Foreground)
            .frame(
                egui::Frame::NONE
                    .fill(theme.card_bg)
                    .corner_radius(12)
                    .stroke(egui::Stroke::new(1.0, theme.card_stroke))
                    .inner_margin(egui::Margin::symmetric(24, 22)),
            )
            .show(ctx, |ui| {
                ui.set_max_width(470.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚠").size(22.0).color(theme.orange));
                    ui.label(
                        egui::RichText::new(t.perm_title)
                            .size(16.0)
                            .strong()
                            .color(theme.text),
                    );
                });
                ui.add_space(10.0);
                ui.label(egui::RichText::new(t.perm_body).size(13.0).color(theme.muted));
                ui.add_space(16.0);

                // Two "open System Settings" buttons, then the primary re-check + dismiss row.
                ui.horizontal_wrapped(|ui| {
                    if secondary_btn(ui, theme, t.perm_open_input) {
                        open_input = true;
                    }
                    ui.add_space(8.0);
                    if secondary_btn(ui, theme, t.perm_open_accessibility) {
                        open_accessibility = true;
                    }
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if primary_btn(ui, theme, t.perm_recheck) {
                        recheck = true;
                    }
                    ui.add_space(8.0);
                    if link_btn(ui, theme, t.perm_dismiss) {
                        dismiss = true;
                    }
                });
            });

        if open_input {
            open_permission_pane("Privacy_ListenEvent");
        }
        if open_accessibility {
            open_permission_pane("Privacy_Accessibility");
        }
        if dismiss {
            self.perm_dismissed = true;
        }
        if recheck {
            // Re-start the capture. Success clears `capture_failed`; failure re-sets it so this
            // dialog stays up until the user actually grants the permission.
            self.perm_dismissed = false;
            self.perm_prompt_sent = false;
            crate::capture::start_capture(self.grab_ctx.clone(), self.capture_failed.clone());
        }
    }
}

/// Open a specific System Settings privacy pane (macOS). No-op on other platforms.
fn open_permission_pane(pane: &str) {
    #[cfg(target_os = "macos")]
    {
        let url = format!("x-apple.systempreferences:com.apple.preference.security?{}", pane);
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pane;
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
        self.drain_notifications();

        // Auto-discovery may have linked us in the background (the listener thread flips `net`
        // to `Secondary`). Clear any stale startup-error banner so the UI reflects the live state.
        if matches!(&*self.net.lock().unwrap(), Net::Secondary { .. }) && self.startup_error.is_some() {
            self.startup_error = None;
        }

        let theme = UiTheme::from_ctx(ctx);

        // Publish the window's screen rect so the event-tap can hand control back when the
        // user clicks inside MouseShare's own UI while a secondary has control (see
        // `GrabCtx::ui_window_rect`).
        if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
            if let Ok(mut g) = self.grab_ctx.ui_window_rect.lock() {
                *g = Some((r.min.x as f64, r.min.y as f64, r.max.x as f64, r.max.y as f64));
            }
        }

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

        // ---- Missing-permission guidance dialog (macOS primary capture tap failed) ----
        // Rendered as a floating modal over the whole window; it polls the capture thread's flag.
        self.show_permission_dialog(ctx, t, theme);

        // The discovery page only makes sense for a secondary (a primary *is* the host); if the
        // role was switched while that page was open, fall back to the connection page.
        if self.page == Page::Discovered && self.config.mode != "secondary" {
            self.page = Page::Connection;
        }

        // ---- Sidebar: real navigation (macOS System Settings pattern) ----
        // A fixed 220px icon+label rail instead of stacked setting cards. Moving the settings
        // into the content area is what frees room for large page titles and full-width cards —
        // the stacked-card sidebar had space for neither.
        egui::SidePanel::left("nav")
            .default_width(232.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(theme.sidebar_bg)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 14,
                        // Clears the macOS traffic lights — see `TITLEBAR_CLEARANCE`.
                        top: TITLEBAR_CLEARANCE,
                        bottom: 14,
                    }),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                // Hairline separating the sidebar material from the content.
                let pr = ui.max_rect();
                ui.painter().line_segment(
                    [pos2(pr.right(), pr.min.y), pos2(pr.right(), pr.max.y)],
                    (1.0, theme.hairline),
                );
                brand_block(ui, theme);
                ui.add_space(14.0);

                let peer_count = self.net.lock().unwrap().peer_count();
                let disc_count = self.discovered.lock().unwrap().len();

                nav_group_label(ui, t.nav_group_config, theme);
                ui.add_space(2.0);
                if nav_item(
                    ui,
                    theme,
                    self.page == Page::Connection,
                    t.nav_connection,
                    NavIcon::Connection,
                    None,
                ) {
                    self.page = Page::Connection;
                }
                if nav_item(ui, theme, self.page == Page::Layout, t.nav_layout, NavIcon::Layout, None) {
                    self.page = Page::Layout;
                }
                let status_badge =
                    if peer_count > 0 { Some((format!("{}", peer_count), theme.accent)) } else { None };
                if nav_item(
                    ui,
                    theme,
                    self.page == Page::Status,
                    t.nav_status,
                    NavIcon::Status,
                    status_badge.as_ref(),
                ) {
                    self.page = Page::Status;
                }

                nav_group_label(ui, t.nav_group_network, theme);
                let disc_badge =
                    if disc_count > 0 { Some((format!("{}", disc_count), theme.green)) } else { None };
                if self.config.mode == "secondary" {
                    if nav_item(
                        ui,
                        theme,
                        self.page == Page::Discovered,
                        t.nav_discovered,
                        NavIcon::Network,
                        disc_badge.as_ref(),
                    ) {
                        self.page = Page::Discovered;
                    }
                }

                // Bottom-anchored footer block: status pill, language chip, then Quit — all
                // pinned to the foot of the rail on tall windows, separated by hairlines.
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add_space(6.0);

                    // Quit row.
                    if nav_item(ui, theme, false, t.exit_app, NavIcon::Power, None) {
                        if self.config.mode == "primary" {
                            self.config.layout = self.shared_layout.lock().unwrap().clone();
                            save_config(&self.config);
                        }
                        std::process::exit(0);
                    }
                    ui.add_space(8.0);

                    // Hairline above the lang chip.
                    let r1 = ui.available_rect_before_wrap();
                    ui.painter().line_segment(
                        [pos2(r1.min.x, r1.min.y), pos2(r1.max.x, r1.min.y)],
                        (1.0, theme.hairline),
                    );
                    ui.add_space(8.0);

                    // Language chip row.
                    if lang_chip(ui, theme, self.lang) {
                        self.lang = self.lang.toggled();
                        self.config.lang = self.lang.code().to_string();
                        crate::i18n::set_lang(self.lang);
                        save_config(&self.config);
                    }
                    ui.add_space(8.0);

                    // Hairline above the status row.
                    let r2 = ui.available_rect_before_wrap();
                    ui.painter().line_segment(
                        [pos2(r2.min.x, r2.min.y), pos2(r2.max.x, r2.min.y)],
                        (1.0, theme.hairline),
                    );
                    ui.add_space(10.0);

                    // Status row: coloured dot + short label on a tint.
                    let (dot_color, status_text, tint) = match &*self.net.lock().unwrap() {
                        Net::Primary { .. } => (theme.accent, t.conn_primary, theme.accent_tint),
                        Net::Secondary { .. } => (theme.green, t.conn_connected, theme.green_tint),
                        Net::Idle => (theme.orange, t.conn_idle, theme.orange_tint),
                    };
                    status_pill(ui, dot_color, tint, status_text);
                    ui.add_space(4.0);
                });
            });

        // ---- Main content: whichever page the sidebar has selected ----
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme.window_bg))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(egui::Margin { left: 48, right: 48, top: 36, bottom: 32 })
                            .show(ui, |ui| match self.page {
                                Page::Connection => self.page_connection(ui, t, theme),
                                Page::Layout => self.page_layout(ui, t, theme),
                                Page::Status => self.page_status(ui, t, theme),
                                Page::Discovered => self.page_discovered(ui, t, theme),
                            });
                    });
            });

        // ---- Transient toast: bottom-centre overlay, click-through ----
        if let Some((_, msg)) = &self.toast {
            let msg = msg.clone();
            egui::Area::new(egui::Id::new("toast"))
                .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -28.0))
                .order(egui::Order::Tooltip)
                .show(ctx, |ui| {
                    egui::Frame::NONE
                        .fill(theme.card_bg)
                        .corner_radius(10)
                        .inner_margin(egui::Margin::symmetric(16, 9))
                        .stroke(egui::Stroke::new(1.0, theme.card_stroke))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("✓").strong().color(theme.green));
                                ui.label(egui::RichText::new(&msg).size(12.5).color(theme.text));
                            });
                        });
                });
        }

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
    // ---- Pages ---------------------------------------------------------------------------

    /// The tail of the diagnostic log, refreshed at most every 2 s. Each entry is
    /// `(age-label, message, dot-colour)`, oldest first.
    fn recent_activity(&mut self, max: usize) -> Vec<(String, String, Color32)> {
        let now = Instant::now();
        let stale = match self.activity_poll {
            Some(t0) => now.duration_since(t0) >= Duration::from_secs(2),
            None => true,
        };
        if stale {
            self.activity_cache = read_activity(max);
            self.activity_poll = Some(now);
        }
        self.activity_cache.clone()
    }

    /// Connection: role (two mode cards), role-specific networking, local screens.
    fn page_connection(&mut self, ui: &mut egui::Ui, t: Tr, theme: UiTheme) {
        page_header(ui, t.page_connection, t.page_connection_sub, theme);

        // ---- Role: two large mode cards (System Settings "default app" pattern) ----
        // A segmented control fit the label but not the explanation; two cards give room for
        // the one-line description that tells a first-time user which role to pick.
        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_role, t.card_role_sub, |_ui| {});
            card_body(ui, |ui| {
                let gap = 12.0;
                let w = ((ui.available_width() - gap) / 2.0).max(140.0);
                let mut picked: Option<&'static str> = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    if mode_card(
                        ui,
                        w,
                        theme,
                        self.config.mode == "primary",
                        t.role_primary_card,
                        t.role_primary_desc,
                        NavIcon::Layout,
                        "role-primary",
                    ) {
                        picked = Some("primary");
                    }
                    if mode_card(
                        ui,
                        w,
                        theme,
                        self.config.mode == "secondary",
                        t.role_secondary_card,
                        t.role_secondary_desc,
                        NavIcon::Connection,
                        "role-secondary",
                    ) {
                        picked = Some("secondary");
                    }
                });
                if let Some(p) = picked {
                    self.config.mode = p.to_string();
                }
            });
        });

        // ---- Role-specific networking ----
        if self.config.mode == "secondary" {
            card(ui, theme, |ui| {
                card_header(ui, theme, t.card_secondary, t.card_secondary_sub, |_ui| {});
                card_body(ui, |ui| {
                    form_row(ui, theme, t.machine_name, true, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.name)
                                .desired_width(f32::INFINITY),
                        );
                    });
                    form_row(ui, theme, t.server_addr, true, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.server_addr)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                    form_row(ui, theme, t.primary_name, false, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.primary_name)
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
                let mut connect = false;
                card_footer(ui, theme, |ui| {
                    connect = primary_btn(ui, theme, t.connect_host);
                });
                if connect {
                    self.reconnect();
                }
            });
        } else {
            let mut detect = false;
            let mut save = false;
            card(ui, theme, |ui| {
                card_header(ui, theme, t.card_primary, t.card_primary_sub, |_ui| {});
                card_body(ui, |ui| {
                    form_row(ui, theme, t.machine_name, true, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.name)
                                .desired_width(f32::INFINITY),
                        );
                    });
                    form_row(ui, theme, t.listen_port, true, |ui| {
                        ui.add(egui::DragValue::new(&mut self.config.port).speed(1));
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new(t.port_hint).size(11.5).color(theme.faint));
                    });
                    let mut copy = false;
                    form_row(ui, theme, t.address, false, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.server_addr)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.add_space(10.0);
                        copy = link_btn(ui, theme, t.copy);
                    });
                    if copy {
                        let addr = self.config.server_addr.clone();
                        clipboard::set_clipboard(&addr);
                        self.show_toast(t.copied);
                    }
                });
                card_footer(ui, theme, |ui| {
                    detect = secondary_btn(ui, theme, t.detect_ip);
                    ui.add_space(8.0);
                    save = primary_btn(ui, theme, t.save);
                });
            });

            // ---- Cross-screen pointer speed ----
            // The ratio itself is computed per hand-off from both machines' scale factors; this
            // card only exposes a manual trim on top of it (and shows the resulting value).
            card(ui, theme, |ui| {
                card_header(ui, theme, t.card_speed, t.card_speed_sub, |_ui| {});
                card_body(ui, |ui| {
                    form_row(ui, theme, t.speed_multiplier, true, |ui| {
                        let before = self.config.motion_scale;
                        ui.add(
                            egui::DragValue::new(&mut self.config.motion_scale)
                                .speed(0.05)
                                .range(0.25..=4.0)
                                .suffix("×"),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(t.speed_hint)
                                .size(11.5)
                                .color(theme.faint),
                        );
                        if (self.config.motion_scale - before).abs() > f32::EPSILON {
                            crate::control::set_motion_scale(self.config.motion_scale);
                            save_config(&self.config);
                        }
                    });
                    // Live readout: auto ratio × manual trim, for the first connected peer.
                    let auto = {
                        let l = self.shared_layout.lock().unwrap();
                        l.screens
                            .iter()
                            .find(|s| !s.is_local)
                            .map(|s| crate::control::motion_scale_ratio(&l, &s.name, None))
                    };
                    form_row(ui, theme, t.speed_effective, false, |ui| {
                        let txt = match auto {
                            Some(r) => format!("{:.2} ×", r),
                            None => t.speed_no_peer.to_string(),
                        };
                        ui.label(
                            egui::RichText::new(txt)
                                .size(12.5)
                                .strong()
                                .color(theme.accent),
                        );
                    });
                });
            });
            if detect {
                if let Ok(ip) = local_ip_address::local_ip() {
                    self.config.server_addr = format!("{}:{}", ip, self.config.port);
                }
            }
            if save {
                self.config.layout = self.shared_layout.lock().unwrap().clone();
                save_config(&self.config);
                self.show_toast(t.saved_hint);
            }
        }

        // ---- Local screens (this machine's own displays) ----
        let mut dup_idx: Option<usize> = None;
        let mut del_idx: Option<usize> = None;
        let mut add = false;
        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_screens, t.card_screens_sub, |ui| {
                add = secondary_btn(ui, theme, t.add_screen);
            });
            card_body(ui, |ui| {
                let layout = self.shared_layout.lock().unwrap();
                if layout.screens.is_empty() {
                    ui.label(egui::RichText::new(t.screens_empty).size(12.5).color(theme.muted));
                    return;
                }
                for (i, s) in layout.screens.iter().enumerate() {
                    let phys = s.physical_size();
                    let meta = if phys != (s.w, s.h) {
                        format!("{} × {} · @{:.0}x", s.w, s.h, s.scale)
                    } else {
                        format!("{} × {}", s.w, s.h)
                    };
                    let joined = s.is_local;
                    peer_row(
                        ui,
                        theme,
                        &s.name,
                        &meta,
                        NavIcon::Layout,
                        if joined { theme.accent } else { theme.muted },
                        |ui| {
                            // Only offer removal when more than one screen would remain.
                            if layout.screens.len() > 1 && danger_link(ui, theme, t.del) {
                                del_idx = Some(i);
                            }
                            ui.add_space(10.0);
                            if link_btn(ui, theme, t.dup) {
                                dup_idx = Some(i);
                            }
                        },
                    );
                }
            });
        });
        if let Some(i) = dup_idx {
            self.shared_layout.lock().unwrap().duplicate_screen(i);
        }
        if let Some(i) = del_idx {
            self.shared_layout.lock().unwrap().screens.remove(i);
        }
        if add {
            let mut layout = self.shared_layout.lock().unwrap();
            let max_x = layout.screens.iter().map(|s| s.ox + s.w as i32).max().unwrap_or(0);
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
    }

    /// Screen layout: the draggable virtual desktop, plus the machine list.
    fn page_layout(&mut self, ui: &mut egui::Ui, t: Tr, theme: UiTheme) {
        page_header(ui, t.page_layout, t.page_layout_sub, theme);

        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_canvas, t.card_canvas_sub, |_ui| {});
            card_body(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), 400.0),
                    egui::Sense::hover(),
                );
                // Recessed surface for the virtual desktop.
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(10), theme.fill_bg);
                dot_grid(ui.painter(), rect, theme.grid);
                ui.painter().rect_stroke(
                    rect,
                    egui::CornerRadius::same(10),
                    (1.0, theme.card_stroke),
                    egui::StrokeKind::Inside,
                );
                ui.set_clip_rect(rect);

                let cur = self.ctrl.lock().unwrap().last_real;
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
                let changed =
                    draw_layout(ui, &mut layout, &self.config.name, t, theme, rect, cur);
                drop(layout);
                // Persist drag repositioning immediately (primary only — it owns the layout and
                // broadcasts it to every secondary within 2 s).
                if changed && self.config.mode == "primary" {
                    self.config.layout = self.shared_layout.lock().unwrap().clone();
                    save_config(&self.config);
                }
            });
        });

        // Machine list: the screens in the layout and whether they are live.
        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_clients, t.card_clients_sub, |_ui| {});
            card_body(ui, |ui| {
                let layout = self.shared_layout.lock().unwrap();
                if layout.screens.is_empty() {
                    ui.label(egui::RichText::new(t.screens_empty).size(12.5).color(theme.muted));
                    return;
                }
                for s in layout.screens.iter() {
                    let phys = s.physical_size();
                    let meta = if phys != (s.w, s.h) {
                        format!("{} × {} · @{:.0}x", s.w, s.h, s.scale)
                    } else {
                        format!("{} × {}", s.w, s.h)
                    };
                    let live = s.name == self.my_name || s.is_local;
                    peer_row(
                        ui,
                        theme,
                        &s.name,
                        &meta,
                        NavIcon::Layout,
                        if live { theme.green } else { theme.muted },
                        |ui| {
                            let (label, col) = if live {
                                (t.online, theme.green)
                            } else {
                                (t.offline, theme.faint)
                            };
                            status_chip(ui, theme, col, label);
                        },
                    );
                }
            });
        });
    }

    /// Status: live session stats, network peers, and the tail of the diagnostic log.
    fn page_status(&mut self, ui: &mut egui::Ui, t: Tr, theme: UiTheme) {
        page_header(ui, t.page_status, t.page_status_sub, theme);

        let peers = self.net.lock().unwrap().peer_count();
        let conn_label = match &*self.net.lock().unwrap() {
            Net::Primary { .. } => t.conn_primary,
            Net::Secondary { .. } => t.conn_connected,
            Net::Idle => t.conn_idle,
        };
        let conn_color = match &*self.net.lock().unwrap() {
            Net::Primary { .. } => theme.accent,
            Net::Secondary { .. } => theme.green,
            Net::Idle => theme.orange,
        };
        let ctrl_line = if self.config.mode == "primary" {
            let c = self.ctrl.lock().unwrap();
            match &c.remote {
                Some(r) => t.ctrl_remote.replace("{}", &r.name),
                None => t.ctrl_local.to_string(),
            }
        } else {
            t.ctrl_local.to_string()
        };

        // ---- Session stats ----
        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_stats, "", |_ui| {});
            card_body(ui, |ui| {
                ui.columns(3, |cols| {
                    stat_tile(&mut cols[0], theme, t.stat_peers, &format!("{}", peers), t.stat_peers_foot);
                    stat_tile(&mut cols[1], theme, t.stat_conn, conn_label, "");
                    stat_tile(&mut cols[2], theme, t.stat_ctrl, &ctrl_line, "");
                });
            });
            // Hints live inside the same card, below the stats.
            card_body(ui, |ui| {
                ui.add_space(12.0);
                ui.label(egui::RichText::new(t.local_name).size(12.5).color(theme.muted));
                ui.label(egui::RichText::new(&self.my_name).size(13.0).strong().color(theme.text));
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(t.hotkey_hint)
                        .size(12.0)
                        .color(theme.muted),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(t.background_hint)
                        .size(12.0)
                        .color(theme.muted),
                );
                if self.config.mode == "primary" {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}",
                            t.diag_hint,
                            crate::diag::log_path().display()
                        ))
                        .size(11.0)
                        .color(theme.faint),
                    );
                }
            });
        });

        // ---- Recent activity: the tail of the real diagnostic log ----
        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_activity, t.card_activity_sub, |_ui| {});
            card_body(ui, |ui| {
                let events = self.recent_activity(6);
                if events.is_empty() {
                    ui.label(egui::RichText::new(t.activity_empty).size(12.5).color(theme.muted));
                    return;
                }
                for (age, msg, dot) in events {
                    activity_row(ui, theme, dot, &age, &msg);
                }
            });
        });

        // ---- Reconnect shortcut for an idle secondary ----
        let mut reconnect = false;
        if self.config.mode == "secondary"
            && matches!(&*self.net.lock().unwrap(), Net::Idle)
        {
            card(ui, theme, |ui| {
                card_header(ui, theme, t.card_network, t.card_network_sub, |_ui| {});
                card_footer(ui, theme, |ui| {
                    reconnect = primary_btn(ui, theme, t.reconnect_host);
                });
            });
        }
        if reconnect {
            self.reconnect();
        }
    }

    /// Discovered primaries on the LAN (secondary only) — one click to connect.
    fn page_discovered(&mut self, ui: &mut egui::Ui, t: Tr, theme: UiTheme) {
        page_header(ui, t.page_discovered, t.page_discovered_sub, theme);

        let list = self.discovered.lock().unwrap().clone();
        let mut pick: Option<String> = None;
        card(ui, theme, |ui| {
            card_header(ui, theme, t.card_discovered, t.card_discovered_sub, |_ui| {});
            card_body(ui, |ui| {
                if list.is_empty() {
                    ui.label(
                        egui::RichText::new(t.discovered_empty)
                            .size(12.5)
                            .color(theme.muted),
                    );
                    return;
                }
                for d in &list {
                    let addr = d.addr();
                    peer_row(
                        ui,
                        theme,
                        &d.name,
                        &addr,
                        NavIcon::Network,
                        theme.green,
                        |ui| {
                            if primary_btn(ui, theme, t.discovered_connect) {
                                pick = Some(addr.clone());
                            }
                        },
                    );
                }
            });
        });
        if let Some(addr) = pick {
            self.config.server_addr = addr.clone();
            self.connect_to(addr);
        }
    }
}

// ---- Sidebar -----------------------------------------------------------------------------

/// Which glyph a sidebar row (or a mode card) draws.
#[derive(Clone, Copy)]
enum NavIcon {
    /// Two displays side by side — sharing / connection.
    Connection,
    /// One display on a stand — screen layout.
    Layout,
    /// Clock face — live status.
    Status,
    /// Two linked nodes — network discovery.
    Network,
    /// Power symbol — quit.
    Power,
}

/// Stroke-drawn 18px navigation glyph. Stroke icons (rather than filled shapes) match the
/// macOS sidebar, and they stay legible at 1.6px in both appearances.
fn draw_nav_icon(p: &egui::Painter, rect: Rect, icon: NavIcon, color: Color32) {
    let s = egui::Stroke::new(1.6, color);
    match icon {
        NavIcon::Connection => {
            let h = rect.height() * 0.66;
            let w = rect.width() * 0.46;
            let y = rect.center().y - h / 2.0;
            let a = Rect::from_min_size(pos2(rect.min.x, y), vec2(w, h));
            let b = Rect::from_min_size(pos2(rect.max.x - w, y + h * 0.20), vec2(w, h * 0.80));
            p.rect_stroke(a, 2.0, s, egui::StrokeKind::Inside);
            p.rect_stroke(b, 2.0, s, egui::StrokeKind::Inside);
        }
        NavIcon::Layout => {
            let body = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height() * 0.70));
            p.rect_stroke(body, 2.0, s, egui::StrokeKind::Inside);
            let cx = rect.center().x;
            p.line_segment([pos2(cx, body.max.y), pos2(cx, rect.max.y - 1.0)], s);
            p.line_segment(
                [
                    pos2(cx - rect.width() * 0.24, rect.max.y - 1.0),
                    pos2(cx + rect.width() * 0.24, rect.max.y - 1.0),
                ],
                s,
            );
        }
        NavIcon::Status => {
            p.circle_stroke(rect.center(), rect.width() * 0.40, s);
            let c = rect.center();
            p.line_segment([c, pos2(c.x, c.y - rect.height() * 0.22)], s);
            p.line_segment([c, pos2(c.x + rect.width() * 0.19, c.y + rect.height() * 0.11)], s);
        }
        NavIcon::Network => {
            let r = rect.width() * 0.15;
            let cy = rect.center().y;
            let c1 = pos2(rect.min.x + r, cy);
            let c2 = pos2(rect.max.x - r, cy);
            p.circle_stroke(c1, r, s);
            p.circle_stroke(c2, r, s);
            p.line_segment([pos2(c1.x + r, cy), pos2(c2.x - r, cy)], s);
        }
        NavIcon::Power => {
            let c = rect.center();
            let r = rect.width() * 0.32;
            // Arc: an open circle with the gap at the top, drawn as short chords.
            let steps = 14;
            let start = -std::f32::consts::PI * 0.80;
            let end = std::f32::consts::PI * 0.80;
            let mut prev: Option<egui::Pos2> = None;
            for i in 0..=steps {
                let a = start + (end - start) * (i as f32 / steps as f32);
                let pt = pos2(c.x + r * a.sin(), c.y - r * a.cos());
                if let Some(pv) = prev {
                    p.line_segment([pv, pt], s);
                }
                prev = Some(pt);
            }
            p.line_segment([pos2(c.x, c.y - r), pos2(c.x, c.y - r * 0.15)], s);
        }
    }
}

/// Small uppercase group heading in the sidebar.
fn nav_group_label(ui: &mut egui::Ui, text: &str, theme: UiTheme) {
    ui.add_space(16.0);
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(text)
            .size(10.5)
            .strong()
            .color(theme.faint),
    );
    ui.add_space(2.0);
}

/// A sidebar row: 18px stroke icon, 13px label, optional right-aligned count badge.
/// Returns `true` when clicked.
fn nav_item(
    ui: &mut egui::Ui,
    theme: UiTheme,
    selected: bool,
    label: &str,
    icon: NavIcon,
    badge: Option<&(String, Color32)>,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(8), theme.nav_active);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(8), theme.nav_hover);
    }
    let icon_rect = Rect::from_min_size(
        pos2(rect.min.x + 10.0, rect.center().y - 9.0),
        vec2(18.0, 18.0),
    );
    draw_nav_icon(
        ui.painter(),
        icon_rect,
        icon,
        if selected { theme.accent } else { theme.muted },
    );
    // Clip so a long label never spills over the badge.
    let cp = ui.painter().with_clip_rect(rect);
    cp.text(
        pos2(rect.min.x + 38.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        theme.text,
    );
    if let Some((text, col)) = badge {
        let w = 12.0 + text.chars().count() as f32 * 6.5;
        let pill = Rect::from_min_size(
            pos2(rect.max.x - 10.0 - w, rect.center().y - 9.0),
            vec2(w, 18.0),
        );
        ui.painter()
            .rect_filled(pill, egui::CornerRadius::same(9), *col);
        ui.painter().text(
            pill.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(10.0),
            Color32::WHITE,
        );
    }
    resp.clicked()
}

/// Top-bar language toggle chip: shows both languages side by side ("中文 / English") with the
/// active one emphasised. Clicking anywhere on the chip switches to the other language. Sits in
/// the toolbar's right group, left of the status pill.
fn lang_chip(ui: &mut egui::Ui, theme: UiTheme, lang: Lang) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(96.0, 26.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(7), theme.nav_hover);
    }
    let cy = rect.center().y;
    let (active, inactive) = (theme.text, theme.muted);
    let (zh_col, en_col, zh_size, en_size) = if lang == Lang::Zh {
        (active, inactive, 12.5, 11.5)
    } else {
        (inactive, active, 11.5, 12.5)
    };
    ui.painter().text(
        pos2(rect.min.x + 10.0, cy),
        Align2::LEFT_CENTER,
        "中文",
        FontId::proportional(zh_size),
        zh_col,
    );
    // Hairline separator between the two labels.
    ui.painter().line_segment(
        [pos2(rect.center().x, cy - 6.0), pos2(rect.center().x, cy + 6.0)],
        (1.0, theme.hairline),
    );
    ui.painter().text(
        pos2(rect.max.x - 10.0, cy),
        Align2::RIGHT_CENTER,
        "English",
        FontId::proportional(en_size),
        en_col,
    );
    resp.clicked()
}

/// Sidebar header: a single muted line of "MouseShare" — nothing more. The earlier version had
/// a 38×38 gradient mark + "MouseShare" wordmark + version number, which read as a logo block
/// rather than a section heading and was the only decoration left in the flat UI.
fn brand_block(ui: &mut egui::Ui, theme: UiTheme) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("MouseShare")
            .size(13.0)
            .strong()
            .color(theme.text),
    );
    ui.add_space(10.0);
}

// ---- Content-area components -------------------------------------------------------------

/// Page title (28pt semibold) + one-line subtitle, with an optional trailing area on the right.
/// Page heading: large title + muted subtitle. Trailing widgets (e.g. action buttons) used to
/// live here; the language toggle has moved to the sidebar footer and other actions live next
/// to the thing they act on, so this header is just typography now.
fn page_header(ui: &mut egui::Ui, title: &str, subtitle: &str, theme: UiTheme) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(title).size(28.0).strong().color(theme.text));
    if !subtitle.is_empty() {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(subtitle).size(13.5).color(theme.muted));
    }
    ui.add_space(28.0);
}

/// A grouped section: no visible frame, just generous vertical rhythm. Earlier versions drew a
/// filled rounded card here; the user asked for a flatter look, so sections are now separated
/// by whitespace and typography rather than borders.
fn card(ui: &mut egui::Ui, _theme: UiTheme, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(20.0);
    body(ui);
}

/// Section title: 16pt semibold, with a muted 12pt subtitle underneath.
fn card_header(
    ui: &mut egui::Ui,
    theme: UiTheme,
    title: &str,
    subtitle: &str,
    action: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                egui::RichText::new(title).size(16.0).strong().color(theme.text),
            );
            if !subtitle.is_empty() {
                ui.label(
                    egui::RichText::new(subtitle).size(12.0).color(theme.muted),
                );
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            action(ui);
        });
    });
    ui.add_space(14.0);
}

/// Section body: standard 4px horizontal padding, roomy vertical breathing room.
fn card_body(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    body(ui);
    ui.add_space(16.0);
}

/// Footer area for actions — same treatment as body, no top divider.
fn card_footer(ui: &mut egui::Ui, _theme: UiTheme, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(body);
}

/// A settings row: 110px label column, then the control, with an optional hairline below.
fn form_row(
    ui: &mut egui::Ui,
    theme: UiTheme,
    label: &str,
    divider: bool,
    control: impl FnOnce(&mut egui::Ui),
) {
    let r = egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(0, 14))
        .show(ui, |ui| {
            let (lr, _) = ui.allocate_exact_size(vec2(130.0, 22.0), egui::Sense::hover());
            let cp = ui.painter().with_clip_rect(lr.expand2(vec2(4.0, 0.0)));
            cp.text(
                lr.left_center(),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(13.0),
                theme.text,
            );
            ui.add_space(18.0);
            control(ui);
        })
        .response
        .rect;
    if divider {
        ui.painter().line_segment(
            [pos2(r.min.x, r.max.y), pos2(r.max.x, r.max.y)],
            (1.0, theme.divider),
        );
    }
}

/// Approximate rendered width of `text` in `font` (sums per-glyph advances). Good enough for
/// wrapping decisions; kerning is a fraction of a pixel at UI sizes.
fn text_width(ui: &egui::Ui, text: &str, font: &FontId) -> f32 {
    ui.fonts(|f| text.chars().map(|c| f.glyph_width(font, c)).sum())
}

/// Greedy wrap. Breaks on ASCII spaces for Latin text and per character for CJK, which has no
/// word separators — a space-only split would never wrap Chinese at all.
fn wrap_lines(ui: &egui::Ui, text: &str, font: &FontId, max_w: f32) -> Vec<String> {
    if max_w <= 0.0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0;
    for seg in text.split(' ') {
        let cjk = seg.chars().any(|c| (c as u32) > 0x2e80);
        if cjk {
            for ch in seg.chars() {
                let w = ui.fonts(|f| f.glyph_width(font, ch));
                if cur_w + w > max_w && !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0.0;
                }
                cur.push(ch);
                cur_w += w;
            }
        } else {
            let w = text_width(ui, seg, font);
            if cur_w + w > max_w && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += ui.fonts(|f| f.glyph_width(font, ' '));
            }
            cur.push_str(seg);
            cur_w += w;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// A selectable "mode" card: 38px icon tile, 15pt title, wrapped 12.5pt description, and a
/// filled accent check in the top-right corner when selected. Two of these side by side is
/// how System Settings presents an either/or choice with consequences worth explaining.
///
/// Everything is drawn by the painter on top of a single click target, so there is no inner
/// widget competing for the click (which is what made the old segmented control fiddly).
/// Role selection row: inline icon + title + wrapped description, with a subtle accent tint
/// behind the selected row. Earlier versions wrapped each option in a filled box with a stroke
/// border, an icon tile, and a filled-circle checkmark — those all read as 'cards' and the
/// user asked for less of that. The accent tint alone is enough to mark the chosen option.
fn mode_card(
    ui: &mut egui::Ui,
    width: f32,
    theme: UiTheme,
    selected: bool,
    title: &str,
    desc: &str,
    icon: NavIcon,
    _id: &str,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 96.0), egui::Sense::click());
    let hovered = resp.hovered();

    // Background: accent tint for the chosen row, hover tint while the user is mousing over the
    // other one. No stroke. The radius is generous so the rect still reads as a discrete row
    // rather than a flat slab, but there is no outer border line.
    if selected {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(8), theme.accent_tint);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(8), theme.nav_hover);
    }

    let icon_color = if selected { theme.accent } else { theme.muted };
    // Inline icon — no tile behind it.
    let icon_rect = Rect::from_center_size(
        pos2(rect.min.x + 26.0, rect.min.y + 24.0),
        vec2(20.0, 20.0),
    );
    draw_nav_icon(ui.painter(), icon_rect, icon, icon_color);

    // Title row.
    ui.painter().text(
        pos2(rect.min.x + 50.0, rect.min.y + 24.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(15.0),
        if selected { theme.text } else { theme.text },
    );

    // Selected indicator: a small accent dot at the right edge, not a filled circle with a check.
    if selected {
        ui.painter().circle_filled(
            pos2(rect.max.x - 18.0, rect.min.y + 24.0),
            4.5,
            theme.accent,
        );
    }

    // Description text wrapped to the row width.
    let pad = 18.0;
    let text_w = (rect.width() - pad * 2.0).max(20.0);
    let cp = ui.painter().with_clip_rect(rect);
    let mut y = rect.min.y + 52.0;
    for line in wrap_lines(ui, desc, &FontId::proportional(12.5), text_w) {
        cp.text(
            pos2(rect.min.x + pad, y),
            Align2::LEFT_TOP,
            line,
            FontId::proportional(12.5),
            theme.muted,
        );
        y += 17.0;
    }
    resp.clicked()
}

/// Filled accent button (the one primary action per card).
fn primary_btn(ui: &mut egui::Ui, theme: UiTheme, label: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(Color32::WHITE))
            .fill(theme.accent)
            .corner_radius(7),
    )
    .clicked()
}

/// Bordered secondary button.
fn secondary_btn(ui: &mut egui::Ui, theme: UiTheme, label: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(theme.text))
            .fill(theme.btn_bg)
            .stroke(egui::Stroke::new(1.0, theme.card_stroke))
            .corner_radius(7),
    )
    .clicked()
}

/// Text-only accent link (used for "Copy", "Duplicate" — cheap actions that shouldn't
/// compete with the primary button).
fn link_btn(ui: &mut egui::Ui, theme: UiTheme, label: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(13.0).color(theme.accent))
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(5),
    )
    .clicked()
}

/// Same as `link_btn` but in the destructive colour.
fn danger_link(ui: &mut egui::Ui, theme: UiTheme, label: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(13.0).color(theme.red))
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(5),
    )
    .clicked()
}

/// A list row for one machine: 36px icon tile, name, monospace metadata, and a trailing slot
/// for an action or status chip.
fn peer_row(
    ui: &mut egui::Ui,
    theme: UiTheme,
    name: &str,
    meta: &str,
    icon: NavIcon,
    tint: Color32,
    trailing: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(0, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (tile, _) = ui.allocate_exact_size(vec2(36.0, 36.0), egui::Sense::hover());
                ui.painter().rect_filled(
                    tile,
                    egui::CornerRadius::same(8),
                    Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), 46),
                );
                draw_nav_icon(ui.painter(), tile.shrink(9.0), icon, tint);

                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    // Allocate both text rows explicitly rather than drawing at `ui.cursor()`:
                    // the cursor rect is zero-height for an empty layout, which makes painter
                    // text drift depending on egui's internal state.
                    let row_w = (ui.available_width() - 90.0).max(40.0);
                    let (nr, _) =
                        ui.allocate_exact_size(vec2(row_w, 17.0), egui::Sense::hover());
                    ui.painter()
                        .with_clip_rect(nr)
                        .text(nr.left_center(), Align2::LEFT_CENTER, name, FontId::proportional(13.0), theme.text);
                    let (mr, _) =
                        ui.allocate_exact_size(vec2(row_w, 14.0), egui::Sense::hover());
                    ui.painter()
                        .with_clip_rect(mr)
                        .text(mr.left_center(), Align2::LEFT_CENTER, meta, FontId::monospace(11.0), theme.muted);
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    trailing(ui);
                });
            });
        });
    // Hairline between rows.
    let r = ui.max_rect();
    ui.painter().line_segment(
        [pos2(r.min.x, r.max.y), pos2(r.max.x, r.max.y)],
        (1.0, theme.divider),
    );
}

/// Small tinted status chip ("在线" / "离线"), used as the trailing element of a list row.
fn status_chip(ui: &mut egui::Ui, theme: UiTheme, color: Color32, label: &str) {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40))
        .corner_radius(9)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                let (dr, _) = ui.allocate_exact_size(vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().circle_filled(dr.center(), 3.5, color);
                ui.label(egui::RichText::new(label).size(11.5).color(theme.text));
            });
        });
}

/// A stat tile: uppercase 11pt label, 18pt value, 11pt footnote on a muted fill.
fn stat_tile(ui: &mut egui::Ui, theme: UiTheme, label: &str, value: &str, foot: &str) {
    ui.add_space(4.0);
    egui::Frame::NONE
        .fill(theme.fill_bg)
        .corner_radius(8)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(label).size(11.0).strong().color(theme.muted));
            ui.add_space(2.0);
            // Explicit rect instead of `ui.cursor()` — see the note in `peer_row`.
            let (vr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), egui::Sense::hover());
            ui.painter()
                .with_clip_rect(vr)
                .text(vr.left_center(), Align2::LEFT_CENTER, value, FontId::proportional(18.0), theme.text);
            if !foot.is_empty() {
                ui.label(egui::RichText::new(foot).size(11.0).color(theme.faint));
            }
        });
}

/// One line of the activity timeline: status dot, relative age, message.
fn activity_row(ui: &mut egui::Ui, theme: UiTheme, dot: Color32, age: &str, msg: &str) {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(0, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dr, _) = ui.allocate_exact_size(vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().circle_filled(dr.center(), 3.5, dot);
                ui.add_space(4.0);
                ui.label(egui::RichText::new(age).size(11.0).color(theme.faint));
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(msg).size(12.0).color(theme.text))
                        .truncate(),
                );
            });
        });
}

/// Read and parse the last `max` entries of the real diagnostic log, oldest first, as
/// `(age-label, message, dot-colour)`. Called at most every 2 s via `recent_activity`.
fn read_activity(max: usize) -> Vec<(String, String, Color32)> {
    let Ok(text) = std::fs::read_to_string(crate::diag::log_path()) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut out = Vec::new();
    for line in text.lines().rev() {
        let Some((ts, msg)) = line.split_once(' ') else { continue };
        let Ok(ms) = ts.parse::<u64>() else { continue };
        let secs = now.saturating_sub(ms) / 1000;
        let age = if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h", secs / 3600)
        };
        // Colour by severity keywords, mirroring the log's own vocabulary.
        let up = msg.to_ascii_uppercase();
        let dot = if up.contains("ERROR") || up.contains("FAIL") {
            Color32::from_rgb(255, 69, 58)
        } else if up.contains("WARN") {
            Color32::from_rgb(255, 159, 10)
        } else if up.contains("HAND-OFF") || up.contains("ENTER") || up.contains("RETURN") {
            Color32::from_rgb(0, 122, 255)
        } else {
            Color32::from_rgb(52, 199, 89)
        };
        out.push((age, msg.to_string(), dot));
        if out.len() >= max {
            break;
        }
    }
    out.reverse();
    out
}

/// A live connection-status pill: a coloured dot followed by a short label on a tinted
/// background — the macOS "status chip" look in the toolbar. The label takes the dot's colour
/// so the three states (serving / connected / idle) read apart at a glance.
fn status_pill(ui: &mut egui::Ui, dot: Color32, tint: Color32, text: &str) {
    // Deliberately built from standard egui widgets instead of a hand-allocated rect. The
    // toolbar lays this out right-to-left beside the language button, and `allocate_rect`
    // ignores the layout direction — it painted the pill straight over the language button.
    // Letting egui place it also auto-fits the width to the text in every language (a fixed
    // estimate based on 'M' width x char count is badly wrong for CJK).
    egui::Frame::NONE
        .fill(tint)
        .corner_radius(12)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // A real circle, not a "●" glyph, so it looks identical in every font.
                let (dot_rect, _) =
                    ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 4.5, dot);
                ui.label(egui::RichText::new(text).size(12.5).color(dot));
            });
        });
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
    // We store the *logical* resolution (w, h) so the label matches the connected-clients list.
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
                (s.w, s.h),
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
            (s.w, s.h),
            s.scale,
            is_primary,
            is_me,
            hover,
            theme,
            top,
            bottom,
            t.legend_me,
        );
    }

    // The actively dragged tile, painted on top of everything.
    if let Some((rect, is_primary, is_me, name, logical, sc)) = dragged {
        let (top, bottom) = tile_colors(is_primary, is_me);
        soft_shadow(ui.painter(), rect, 16.0, theme.shadow, 2.0);
        paint_tile(
            ui.painter(),
            rect,
            &name,
            logical,
            sc,
            is_primary,
            is_me,
            true,
            theme,
            top,
            bottom,
            t.legend_me,
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
        theme.muted,
    );
    changed
}

/// Paint one screen tile as a **display card**, not a coloured slab: a white card with a
/// rounded mini-preview at the top and the machine name / resolution underneath. The old
/// version filled the whole tile with a saturated gradient and scrimmed the bottom, which
/// read as a glowing button rather than a monitor.
///
/// `top`/`bottom` only colour the preview strip now, so a tile stays legible at any size and
/// the accent is reserved for "this machine".
///
/// `logical` is the screen size in OS logical points (the same space used for layout and
/// crossing). We intentionally display logical resolution so the canvas labels match the
/// "connected clients" list and macOS System Settings.
#[allow(clippy::too_many_arguments)]
fn paint_tile(
    painter: &egui::Painter,
    rect: Rect,
    name: &str,
    logical: (u32, u32),
    scale: f32,
    is_primary: bool,
    is_me: bool,
    hover: bool,
    theme: UiTheme,
    top: Color32,
    bottom: Color32,
    me_label: &str,
) {
    const R: f32 = 10.0;

    // 1. Card body: the card surface, one step above the canvas.
    painter.rect_filled(rect, egui::CornerRadius::same(R as u8), theme.card_bg);

    // 2. Border: hairline normally, accent for this machine, accent + halo while hovered.
    let (sw, sc) = if hover {
        (2.0, theme.accent)
    } else if is_me {
        (1.5, theme.accent)
    } else {
        (1.0, Color32::from_gray(209))
    };
    painter.rect_stroke(rect, egui::CornerRadius::same(R as u8), (sw, sc), egui::StrokeKind::Inside);
    if hover {
        painter.rect_stroke(
            rect.expand(4.0),
            egui::CornerRadius::same((R + 4.0) as u8),
            (2.0, Color32::from_rgba_unmultiplied(theme.accent.r(), theme.accent.g(), theme.accent.b(), 120)),
            egui::StrokeKind::Outside,
        );
    }

    let pad = 14.0;
    let content = rect.shrink(pad);
    if content.width() < 24.0 || content.height() < 20.0 {
        return;
    }
    // Clip so long machine names can never spill outside the card.
    let cp = painter.with_clip_rect(rect);

    // 3. Mini preview: a tinted strip with two faint "content" bars, so the tile reads as a
    //    screen even at a glance.
    let prev_h = (content.height() * 0.45).clamp(14.0, 64.0);
    let prev = Rect::from_min_size(content.min, vec2(content.width(), prev_h));
    if prev.height() > 8.0 {
        fill_gradient(&cp, prev, 6.0, top, bottom);
        let lw1 = (prev.width() - 40.0).max(6.0);
        let lw2 = (prev.width() - 62.0).max(4.0);
        let ly1 = prev.min.y + prev.height() * 0.28;
        let ly2 = prev.min.y + prev.height() * 0.50;
        if prev.height() > 18.0 {
            cp.rect_filled(
                Rect::from_min_size(pos2(prev.min.x + 12.0, ly1), vec2(lw1, 4.0)),
                2.0,
                Color32::from_white_alpha(150),
            );
            cp.rect_filled(
                Rect::from_min_size(pos2(prev.min.x + 12.0, ly2), vec2(lw2, 4.0)),
                2.0,
                Color32::from_white_alpha(95),
            );
        }
    }

    // 4. Name + resolution beneath the preview.
    let text_y = prev.max.y + 10.0;
    if text_y < rect.max.y - 4.0 {
        let title = if is_primary { format!("★ {name}") } else { name.to_string() };
        cp.text(
            pos2(content.min.x, text_y),
            Align2::LEFT_TOP,
            title,
            FontId::proportional(13.0),
            theme.text,
        );
        // Show logical resolution (matches the connected-clients list and macOS System Settings).
        // The tile's visual size is already scaled by `scale`; the label should not double it.
        let res = if scale != 1.0 {
            format!("{} × {} · @{:.0}x", logical.0, logical.1, scale)
        } else {
            format!("{} × {}", logical.0, logical.1)
        };
        if text_y + 20.0 < rect.max.y {
            cp.text(
                pos2(content.min.x, text_y + 18.0),
                Align2::LEFT_TOP,
                res,
                FontId::monospace(11.0),
                theme.muted,
            );
        }
    }

    // 5. "This machine" badge floating on the top-left corner — the prototype's white pill,
    //    which stays crisp at any tile size (the old bottom-anchored text collided with the
    //    resolution line on short tiles).
    if is_me && rect.width() > 84.0 && rect.height() > 44.0 {
        let pill = Rect::from_min_size(
            pos2(rect.min.x + 14.0, rect.min.y - 9.0),
            vec2(38.0, 18.0),
        );
        painter.rect_filled(pill, egui::CornerRadius::same(9), theme.card_bg);
        painter.rect_stroke(
            pill,
            egui::CornerRadius::same(9),
            (1.0, theme.accent),
            egui::StrokeKind::Inside,
        );
        painter.text(
            pill.center(),
            Align2::CENTER_CENTER,
            me_label,
            FontId::proportional(9.5),
            theme.accent,
        );
    }
}
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
