//! The design system.
//!
//! Everything visual lives here: the colour/motion tokens, font + style installation, and the
//! small vocabulary of widgets the pages are assembled from. `app.rs` holds application state and
//! *composes* these widgets; it never hard-codes a colour or a pixel value.
//!
//! ## Why a token layer at all
//!
//! The UI is drawn by egui, which has no theme cascade — every `painter.text(...)` call needs an
//! explicit size and colour. Without a single source of truth that turns into dozens of slightly
//! different greys and paddings, which is exactly what makes an app look improvised. `Theme`
//! fixes the palette, [`typography`] fixes the type scale, and the `SP_*` / `R_*` constants fix
//! the rhythm.
//!
//! ## Layout model
//!
//! * **Sidebar** — fixed 236 pt rail: brand, grouped navigation, then a footer block that holds
//!   connection state, the language switch and Quit.
//! * **Content** — a single centred column capped at [`CONTENT_MAX_W`]. A settings window that
//!   stretches a two-field form across 1600 pt is the single most common way a desktop app looks
//!   cheap; capping the measure keeps line lengths readable on any window size.
//! * **Sections** — every group of controls sits in a card: white surface, one hairline border,
//!   12 pt radius. One card style, used everywhere. The card is what makes a page scannable; the
//!   rule is only that a card must *group something*, never that it decorates.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Sense};
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------------------------

/// Vertical clearance for the macOS traffic-light cluster (close / minimise / zoom).
///
/// The window runs with `fullsize_content_view(true)` + `titlebar_shown(false)`, so those buttons
/// float *on top of* the egui canvas instead of living in a reserved strip. They occupy roughly
/// y ∈ [8, 24], x ∈ [12, 76]. Anything drawn in the sidebar's top-left corner without this
/// clearance ends up underneath them — unreadable and unclickable, because the buttons take the
/// hit. Windows and Linux draw a normal title bar, so they need none of it.
#[cfg(target_os = "macos")]
pub const TITLEBAR_CLEARANCE: i8 = 38;
#[cfg(not(target_os = "macos"))]
pub const TITLEBAR_CLEARANCE: i8 = 14;

/// Width of the navigation rail.
pub const SIDEBAR_W: f32 = 236.0;
/// Maximum width of the content column. Wider windows get margin, not longer lines.
pub const CONTENT_MAX_W: f32 = 860.0;
/// Minimum breathing room between the content column and the window edge.
pub const CONTENT_MIN_PAD: f32 = 32.0;

/// Height of a navigation row.
pub const ROW_H: f32 = 32.0;
/// Height of an input or a button. One value, so controls line up across pages.
pub const CTRL_H: f32 = 28.0;
/// Width of the left label column in a form row.
pub const LABEL_W: f32 = 128.0;
/// Default width of a text field. Fields are never stretched to the full column width.
pub const FIELD_W: f32 = 288.0;

/// Corner radius of controls (fields, buttons, rows).
pub const R_CTRL: u8 = 7;
/// Corner radius of cards and popovers.
pub const R_CARD: u8 = 12;

/// Spacing rhythm, on a 4 pt grid. Anything larger than 24 is a section break.
pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
pub const SP_5: f32 = 20.0;
pub const SP_6: f32 = 24.0;
pub const SP_8: f32 = 32.0;

// ---------------------------------------------------------------------------------------------
// Typography
// ---------------------------------------------------------------------------------------------

/// The type scale.
///
/// Note that egui cannot synthesise bold: `RichText::strong()` only swaps in a heavier *colour*.
/// The bundled CJK face ships a single weight, so hierarchy has to come from **size and colour**,
/// never from a weight axis. `TITLE` therefore has to be genuinely large to read as a title, and
/// muted greys carry the secondary levels.
pub mod typography {
    /// Relative timestamps, badges, footnotes.
    pub const CAPTION: f32 = 11.0;
    /// Form labels, list subtitles, hints.
    pub const LABEL: f32 = 12.0;
    /// Default running text and button labels.
    pub const BODY: f32 = 13.0;
    /// Card and section headings.
    pub const SECTION: f32 = 14.0;
    /// The number in a stat tile.
    pub const METRIC: f32 = 20.0;
    /// Page title. The largest type in the app.
    pub const TITLE: f32 = 22.0;
}

// ---------------------------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------------------------

/// Every colour the UI is allowed to use.
///
/// Values track the macOS system palette (`systemBlue`, `label`, `secondaryLabel`,
/// `windowBackground`, `sidebarBackground`, the `system*Fill` greys) so the app reads as native in
/// both appearances. The surfaces are deliberately *layered* — sidebar behind, window, then cards
/// on top — because a flat single-tone window reads as a web page, not a desktop app.
#[derive(Clone, Copy)]
pub struct Theme {
    // -- surfaces, back to front
    /// The content area behind the cards.
    pub window: Color32,
    /// Navigation rail material.
    pub sidebar: Color32,
    /// Card and control surface.
    pub surface: Color32,
    /// Hover / quiet fill on top of `surface`.
    pub surface_alt: Color32,
    /// Recessed wells: the layout canvas, inset groupings.
    pub recessed: Color32,
    /// Text input background — a recessed fill, deliberately a step away from `surface`.
    pub field: Color32,
    /// Text input fill on hover.
    pub field_hover: Color32,
    /// The accent-tinted fill for the selected sidebar row and selected choice cards.
    pub selected_fill: Color32,
    /// Fill for the boot-failure banner.
    ///
    /// Unlike the other `*_soft` tokens — which are translucent because they always land on top of
    /// an opaque card — this one is a *panel* fill. The banner is the first thing painted each
    /// frame, so it composites against freshly-cleared pixels rather than the window surface; a
    /// translucent red there resolves to near-black instead of a pale tint. Hence: pre-composited
    /// over `window`, opaque.
    pub banner_error: Color32,

    // -- lines
    /// Card and control borders.
    pub border: Color32,
    /// The fainter line: inset dividers, separators inside a card.
    pub border_soft: Color32,

    // -- text
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    /// Text drawn on top of `accent`.
    pub on_accent: Color32,

    // -- accent
    pub accent: Color32,
    pub accent_hover: Color32,
    /// 1.5 pt border around a focused/selected control.
    pub accent_line: Color32,

    // -- semantic status
    pub green: Color32,
    pub green_soft: Color32,
    pub orange: Color32,
    pub orange_soft: Color32,
    pub red: Color32,
    pub red_soft: Color32,

    // -- canvas
    pub grid: Color32,

    // -- interaction
    pub nav_hover: Color32,
    pub shadow: Color32,
}

impl Theme {
    pub fn from_ctx(ctx: &egui::Context) -> Self {
        if ctx.style().visuals.dark_mode {
            Self::dark()
        } else {
            Self::light()
        }
    }

    fn light() -> Self {
        let accent = Color32::from_rgb(0, 122, 255);
        let window = Color32::from_rgb(242, 242, 247);
        Self {
            window,
            sidebar: Color32::from_rgb(233, 233, 239),
            surface: Color32::WHITE,
            surface_alt: Color32::from_rgb(242, 242, 247),
            recessed: Color32::from_rgb(245, 245, 249),
            field: Color32::from_rgb(240, 240, 244),
            field_hover: Color32::from_rgb(232, 232, 238),
            selected_fill: tint(accent, 24),
            banner_error: mix(window, Color32::from_rgb(255, 59, 48), 34.0 / 255.0),

            border: Color32::from_black_alpha(26),
            border_soft: Color32::from_black_alpha(13),

            text: Color32::from_rgb(28, 28, 30),
            muted: Color32::from_rgb(126, 126, 134),
            faint: Color32::from_rgb(168, 168, 176),
            on_accent: Color32::WHITE,

            accent,
            accent_hover: Color32::from_rgb(0, 106, 224),
            accent_line: tint(accent, 130),

            green: Color32::from_rgb(40, 176, 76),
            green_soft: Color32::from_rgba_unmultiplied(48, 199, 89, 38),
            orange: Color32::from_rgb(206, 118, 0),
            orange_soft: Color32::from_rgba_unmultiplied(255, 149, 0, 40),
            red: Color32::from_rgb(214, 45, 35),
            red_soft: Color32::from_rgba_unmultiplied(255, 59, 48, 34),

            grid: Color32::from_black_alpha(24),
            nav_hover: Color32::from_black_alpha(11),
            shadow: Color32::from_black_alpha(38),
        }
    }

    fn dark() -> Self {
        let accent = Color32::from_rgb(10, 132, 255);
        let window = Color32::from_rgb(30, 30, 33);
        Self {
            window,
            sidebar: Color32::from_rgb(23, 23, 26),
            surface: Color32::from_rgb(42, 42, 46),
            surface_alt: Color32::from_rgb(54, 54, 59),
            recessed: Color32::from_rgb(25, 25, 28),
            field: Color32::from_rgb(25, 25, 28),
            field_hover: Color32::from_rgb(33, 33, 37),
            selected_fill: tint(accent, 52),
            banner_error: mix(window, Color32::from_rgb(255, 69, 58), 50.0 / 255.0),

            border: Color32::from_white_alpha(24),
            border_soft: Color32::from_white_alpha(13),

            text: Color32::from_rgb(240, 240, 245),
            muted: Color32::from_rgb(150, 150, 158),
            faint: Color32::from_rgb(108, 108, 116),
            on_accent: Color32::WHITE,

            accent,
            accent_hover: Color32::from_rgb(35, 148, 255),
            accent_line: tint(accent, 140),

            green: Color32::from_rgb(66, 214, 106),
            green_soft: Color32::from_rgba_unmultiplied(48, 209, 88, 52),
            orange: Color32::from_rgb(255, 172, 56),
            orange_soft: Color32::from_rgba_unmultiplied(255, 159, 10, 52),
            red: Color32::from_rgb(255, 96, 86),
            red_soft: Color32::from_rgba_unmultiplied(255, 69, 58, 50),

            grid: Color32::from_white_alpha(20),
            nav_hover: Color32::from_white_alpha(14),
            shadow: Color32::from_black_alpha(150),
        }
    }

    /// The tinted background behind a status dot / status label pair.
    pub fn soft_for(&self, c: Color32) -> Color32 {
        if c == self.accent {
            self.selected_fill
        } else if c == self.green {
            self.green_soft
        } else if c == self.orange {
            self.orange_soft
        } else if c == self.red {
            self.red_soft
        } else {
            self.surface_alt
        }
    }
}

/// `base` at `alpha`/255 opacity, keeping its RGB. Used to derive tints.
pub fn tint(base: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha)
}

/// Linear blend between two colours.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t) as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

// ---------------------------------------------------------------------------------------------
// Fonts and global style
// ---------------------------------------------------------------------------------------------

/// Install the font stack.
///
/// Order matters more than it looks. Each family is a *fallback chain*: egui walks it and takes
/// the first face that has the glyph. The bundled Noto Sans SC must come **before** egui's built-in
/// Latin faces, because those contain a handful of full-width CJK punctuation glyphs drawn with
/// Latin metrics — a full-width「（」or「：」picked from them renders as a tiny raised mark instead
/// of the proper centred ideographic form. That single ordering mistake is why Chinese labels used
/// to look subtly broken.
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    #[cfg(target_os = "macos")]
    let system_ui: Option<Vec<u8>> = std::fs::read("/System/Library/Fonts/SFNS.ttf").ok();
    #[cfg(not(target_os = "macos"))]
    let system_ui: Option<Vec<u8>> = None;

    let embedded: &[u8] = include_bytes!("../resources/NotoSansSC-Regular.otf");
    let has_cjk = !embedded.is_empty();
    if has_cjk {
        fonts.font_data.insert(
            "cjk".into(),
            Arc::new(egui::FontData::from_owned(embedded.to_vec())),
        );
    }
    if let Some(b) = &system_ui {
        fonts
            .font_data
            .insert("system-ui".into(), Arc::new(egui::FontData::from_owned(b.clone())));
    }

    // Proportional: system UI face, then CJK, then egui's defaults (emoji and the rest).
    {
        let list = fonts.families.entry(egui::FontFamily::Proportional).or_default();
        if has_cjk && !list.iter().any(|f| f == "cjk") {
            list.insert(0, "cjk".into());
        }
        if system_ui.is_some() && !list.iter().any(|f| f == "system-ui") {
            list.insert(0, "system-ui".into());
        }
    }

    // Monospace keeps its fixed-width face first — addresses and ports must stay aligned — with
    // CJK inserted after it and the system UI face last as a catch-all.
    {
        let list = fonts.families.entry(egui::FontFamily::Monospace).or_default();
        let at = list
            .iter()
            .position(|f| f == "Hack")
            .map(|i| i + 1)
            .unwrap_or(0);
        if has_cjk && !list.iter().any(|f| f == "cjk") {
            list.insert(at, "cjk".into());
        }
        if system_ui.is_some() && !list.iter().any(|f| f == "system-ui") {
            list.push("system-ui".into());
        }
    }

    // Last-resort system CJK faces, for builds where the bundled font is missing.
    if !has_cjk {
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            for p in [
                "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
                "/System/Library/Fonts/PingFang.ttc",
                "C:/Windows/Fonts/msyh.ttc",
                "C:/Windows/Fonts/msyh.ttf",
                "C:/Windows/Fonts/simhei.ttf",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            ] {
                if let Ok(bytes) = std::fs::read(p) {
                    fonts.font_data.insert(
                        "cjk-fallback".into(),
                        Arc::new(egui::FontData::from_owned(bytes)),
                    );
                    let list = fonts.families.entry(fam).or_default();
                    if !list.iter().any(|f| f == "cjk-fallback") {
                        list.push("cjk-fallback".into());
                    }
                    break;
                }
            }
        }
    }

    ctx.set_fonts(fonts);
}

/// Install the global egui style.
///
/// This covers the widgets egui draws for us — `TextEdit`, `DragValue`, scrollbars, tooltips. The
/// rest of the app paints itself through the widgets below, but those built-ins have to be brought
/// in line or they show up in default egui grey next to a themed UI.
pub fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let theme = Theme::from_ctx(ctx);

    style.spacing.item_spacing = vec2(SP_2, SP_2);
    style.spacing.button_padding = vec2(12.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.indent = 18.0;
    style.spacing.window_margin = egui::Margin::same(0);

    // Scroll bars: macOS-style overlay pills.
    //
    // egui's floating preset takes the handle colour from the *widget foreground*, which for us is
    // the text colour. Left alone that paints a near-black hairline (light) / near-white hairline
    // (dark) 2 pt wide, flush against the window edge — it reads as a stray border, not as a
    // scroll bar, and it is the first thing the eye catches on a long page. Widen it, pull it in
    // off the edge, and dial the opacity down so it behaves like a control you summon.
    let scroll = &mut style.spacing.scroll;
    scroll.floating = true;
    scroll.bar_width = 10.0;
    scroll.floating_width = 5.0;
    scroll.bar_outer_margin = 3.0;
    scroll.handle_min_length = 36.0;
    scroll.dormant_handle_opacity = 0.0;
    scroll.active_handle_opacity = 0.34;
    scroll.interact_handle_opacity = 0.72;

    style.spacing.interact_size.y = CTRL_H;

    style.text_styles = [
        (egui::TextStyle::Small, FontId::proportional(typography::CAPTION)),
        (egui::TextStyle::Body, FontId::proportional(typography::BODY)),
        (egui::TextStyle::Button, FontId::proportional(typography::BODY)),
        (egui::TextStyle::Heading, FontId::proportional(typography::SECTION)),
        (egui::TextStyle::Monospace, FontId::monospace(12.0)),
    ]
    .into_iter()
    .collect();

    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
        &mut style.visuals.widgets.noninteractive,
    ] {
        w.corner_radius = egui::CornerRadius::same(R_CTRL);
        w.expansion = 0.0;
    }

    // Inputs wear a recessed fill and no border, rather than white-on-white with a hairline: a
    // 1 px border at 10 % alpha is invisible against a white card, which made the text fields
    // read as plain labels instead of something you can type into. Focus is the accent ring.
    style.visuals.widgets.inactive.bg_fill = theme.field;
    style.visuals.widgets.inactive.weak_bg_fill = theme.field;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, theme.text);

    style.visuals.widgets.hovered.bg_fill = theme.field_hover;
    style.visuals.widgets.hovered.weak_bg_fill = theme.field_hover;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, theme.text);

    style.visuals.widgets.active.bg_fill = theme.field;
    style.visuals.widgets.active.weak_bg_fill = theme.field;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, theme.accent);
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, theme.text);

    style.visuals.widgets.noninteractive.bg_fill = theme.surface;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, theme.border_soft);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, theme.text);

    style.visuals.selection.bg_fill = tint(theme.accent, 90);
    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, theme.text);
    style.visuals.extreme_bg_color = theme.field;
    style.visuals.faint_bg_color = theme.surface_alt;
    style.visuals.window_fill = theme.surface;
    style.visuals.panel_fill = theme.window;
    style.visuals.window_stroke = egui::Stroke::new(1.0_f32, theme.border);
    style.visuals.window_corner_radius = egui::CornerRadius::same(R_CARD);
    style.visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: theme.shadow,
    };
    style.visuals.override_text_color = Some(theme.text);
    style.visuals.handle_shape = egui::style::HandleShape::Circle;

    ctx.set_style(style);
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Width of `text` laid out as a single line.
///
/// Public because anything painted straight onto a painter (screen tiles, stat values) has to
/// measure by hand before deciding where the next element goes.
pub fn measure(ui: &egui::Ui, text: &str, font: &FontId) -> f32 {
    text_width(ui, text, font)
}

/// Lay out a single line and return its width.
fn text_width(ui: &egui::Ui, text: &str, font: &FontId) -> f32 {
    ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE))
        .size()
        .x
}

/// Shorten `text` with a trailing ellipsis until it fits `max_w`.
///
/// egui's `Label::truncate()` only works for widgets laid out through `Label`; anything painted
/// straight onto a painter (stat values, canvas tiles, nav badges) has to be measured by hand —
/// otherwise a long machine name silently runs off the edge of its box.
pub fn ellipsize(ui: &egui::Ui, text: &str, font: &FontId, max_w: f32) -> String {
    if text_width(ui, text, font) <= max_w {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>() + "…";
        if text_width(ui, &candidate, font) <= max_w {
            return candidate;
        }
    }
    "…".to_owned()
}

/// Wrap `text` into lines that fit `max_w`, honouring both scripts.
///
/// Splitting on spaces only works for latin text. Chinese and Japanese are written without
/// spaces between words, so a space-based wrapper treats an entire sentence as one token and
/// emits a single line that runs off the edge of its box. This walks characters instead, and
/// merely *prefers* to break at the last space when the line has one — which keeps English
/// looking right without mangling CJK.
pub fn wrap_lines(ui: &egui::Ui, text: &str, font: &FontId, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        let mut candidate = cur.clone();
        candidate.push(ch);
        if text_width(ui, &candidate, font) <= max_w || cur.is_empty() {
            cur = candidate;
            continue;
        }
        // The line is full. Break at the last space if that leaves something sensible,
        // otherwise break right here (the CJK case).
        match cur.rfind(' ') {
            Some(i) if i > 0 => {
                let carry = cur[i + 1..].to_owned();
                lines.push(cur[..i].to_owned());
                cur = carry;
                cur.push(ch);
            }
            _ => {
                lines.push(std::mem::take(&mut cur));
                cur.push(ch);
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

// ---------------------------------------------------------------------------------------------
// Painting helpers
// ---------------------------------------------------------------------------------------------

/// Fill a rounded rect with a vertical gradient, as `rows` horizontal bands.
pub fn fill_gradient(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    top: Color32,
    bottom: Color32,
) {
    let rows = (rect.height().round() as usize).clamp(1, 64);
    let h = rect.height() / rows as f32;
    for i in 0..rows {
        let t = i as f32 / (rows.saturating_sub(1).max(1)) as f32;
        let band = Rect::from_min_size(
            pos2(rect.min.x, rect.min.y + i as f32 * h),
            vec2(rect.width(), h + 0.5),
        );
        // Only the first and last bands need the rounded ends; the middle can be square, which
        // keeps the corner arc from being redrawn 60 times.
        let r = if i == 0 || i == rows - 1 { radius } else { 0.0 };
        painter.rect_filled(band, egui::CornerRadius::same(r as u8), mix(top, bottom, t));
    }
}

/// A soft drop shadow, approximated by concentric translucent rounded rects.
pub fn soft_shadow(painter: &egui::Painter, rect: Rect, radius: f32, color: Color32, lift: f32) {
    let steps = 6;
    for i in (0..steps).rev() {
        let t = (i + 1) as f32 / steps as f32;
        let grow = lift * t;
        let a = (color.a() as f32 * (1.0 - t) * 0.5) as u8;
        painter.rect_filled(
            rect.expand(grow).translate(vec2(0.0, grow * 0.35)),
            egui::CornerRadius::same((radius + grow) as u8),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a),
        );
    }
}

/// The faint dot grid used as the virtual-desktop backdrop.
pub fn dot_grid(painter: &egui::Painter, rect: Rect, color: Color32) {
    let step = 22.0;
    let r = 0.9;
    let mut y = rect.min.y + step;
    while y < rect.max.y {
        let mut x = rect.min.x + step;
        while x < rect.max.x {
            painter.circle_filled(pos2(x, y), r, color);
            x += step;
        }
        y += step;
    }
}

/// Gradient endpoints for a screen tile, keyed by role.
///
/// `is_hub` marks the panels of the machine acting as primary (`Screen::is_local`, which the hub
/// sets and the broadcast layout preserves, so it means the same thing on every machine);
/// `is_mine` marks the panels of the machine *running this instance* (`Screen::host() == my_name`).
/// The two differ on a client — its own display is `is_mine` but not `is_hub` — and on a machine
/// with several monitors, where *every* panel is `is_mine`.
///
/// `is_mine` wins, because "which tile is my computer" is the question the canvas has to answer
/// first; telling the hub apart is the fallback that keeps the remaining greys readable.
pub fn tile_colors(is_hub: bool, is_mine: bool) -> (Color32, Color32) {
    if is_mine {
        (Color32::from_rgb(64, 156, 255), Color32::from_rgb(0, 106, 224))
    } else if is_hub {
        (Color32::from_rgb(120, 130, 150), Color32::from_rgb(84, 94, 112))
    } else {
        (Color32::from_rgb(146, 156, 176), Color32::from_rgb(108, 118, 138))
    }
}

// ---------------------------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------------------------

/// A stroke-drawn glyph. Stroke rather than filled shapes, at a consistent 1.5 pt, so icons stay
/// legible in both appearances and match the weight of the surrounding text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Two displays side by side — connection and role.
    Displays,
    /// One display on a stand — screen layout.
    Monitor,
    /// Clock face — live status.
    Clock,
    /// Two linked nodes — network discovery.
    Link,
    /// Power symbol — quit.
    Power,
}

pub fn draw_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = egui::Stroke::new(1.5_f32, color);
    let c = rect.center();
    match icon {
        Icon::Displays => {
            let h = rect.height() * 0.62;
            let w = rect.width() * 0.44;
            let y = c.y - h / 2.0;
            let a = Rect::from_min_size(pos2(rect.min.x, y), vec2(w, h));
            let b = Rect::from_min_size(pos2(rect.max.x - w, y + h * 0.18), vec2(w, h * 0.82));
            painter.rect_stroke(a, 2.0, s, egui::StrokeKind::Inside);
            painter.rect_stroke(b, 2.0, s, egui::StrokeKind::Inside);
        }
        Icon::Monitor => {
            let body = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height() * 0.68));
            painter.rect_stroke(body, 2.0, s, egui::StrokeKind::Inside);
            painter.line_segment([pos2(c.x, body.max.y), pos2(c.x, rect.max.y - 1.0)], s);
            painter.line_segment(
                [
                    pos2(c.x - rect.width() * 0.22, rect.max.y - 1.0),
                    pos2(c.x + rect.width() * 0.22, rect.max.y - 1.0),
                ],
                s,
            );
        }
        Icon::Clock => {
            painter.circle_stroke(c, rect.width() * 0.42, s);
            painter.line_segment([c, pos2(c.x, c.y - rect.height() * 0.22)], s);
            painter.line_segment([c, pos2(c.x + rect.width() * 0.18, c.y + rect.height() * 0.11)], s);
        }
        Icon::Link => {
            let r = rect.width() * 0.15;
            let n1 = pos2(rect.min.x + r, c.y);
            let n2 = pos2(rect.max.x - r, c.y);
            painter.circle_stroke(n1, r, s);
            painter.circle_stroke(n2, r, s);
            painter.line_segment([pos2(n1.x + r, c.y), pos2(n2.x - r, c.y)], s);
        }
        Icon::Power => {
            let r = rect.width() * 0.32;
            let steps = 14;
            let start = -std::f32::consts::PI * 0.80;
            let end = std::f32::consts::PI * 0.80;
            let mut prev: Option<egui::Pos2> = None;
            for i in 0..=steps {
                let a = start + (end - start) * (i as f32 / steps as f32);
                let pt = pos2(c.x + r * a.sin(), c.y - r * a.cos());
                if let Some(pv) = prev {
                    painter.line_segment([pv, pt], s);
                }
                prev = Some(pt);
            }
            painter.line_segment([pos2(c.x, c.y - r), pos2(c.x, c.y - r * 0.15)], s);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------------------------

/// Button weight. The hierarchy is deliberate: one primary action per section, everything else
/// secondary, tertiary actions rendered as quiet text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// Filled accent. At most one per screen region.
    Primary,
    /// Bordered surface. The default.
    Secondary,
    /// Borderless, accent-coloured text. Inline actions inside a row.
    Quiet,
    /// Borderless red text. Destructive inline actions.
    Danger,
}

impl Btn {
    fn colors(self, th: &Theme, hovered: bool, pressed: bool) -> (Color32, Color32, Option<Color32>) {
        match self {
            Btn::Primary => {
                let bg = if pressed {
                    th.accent_hover
                } else if hovered {
                    mix(th.accent, th.accent_hover, 0.6)
                } else {
                    th.accent
                };
                (bg, th.on_accent, None)
            }
            Btn::Secondary => {
                let bg = if pressed || hovered { th.surface_alt } else { th.surface };
                (bg, th.text, Some(if hovered { th.accent_line } else { th.border }))
            }
            Btn::Quiet => {
                let bg = if pressed || hovered { th.nav_hover } else { Color32::TRANSPARENT };
                (bg, th.accent, None)
            }
            Btn::Danger => {
                let bg = if pressed || hovered { th.red_soft } else { Color32::TRANSPARENT };
                (bg, th.red, None)
            }
        }
    }
}

/// A button sized to its label. Returns `true` on the click that completes.
pub fn button(ui: &mut egui::Ui, th: &Theme, label: &str, kind: Btn) -> bool {
    let font = FontId::proportional(typography::BODY);
    let pad = if kind == Btn::Quiet || kind == Btn::Danger { SP_2 } else { 14.0 };
    let w = text_width(ui, label, &font) + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, CTRL_H), Sense::click());
    let (bg, fg, stroke) = kind.colors(th, resp.hovered(), resp.is_pointer_button_down_on());
    if kind == Btn::Primary || bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(R_CTRL), bg);
    }
    if let Some(sc) = stroke {
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(R_CTRL),
            (1.0_f32, sc),
            egui::StrokeKind::Inside,
        );
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}


// ---------------------------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------------------------

/// A section card: title, optional subtitle, optional right-aligned action, then the body.
///
/// Exactly one card style exists, and every grouping in the app uses it. The consistency is the
/// point — pages read as the same application rather than as a pile of one-off layouts.
pub fn section(
    ui: &mut egui::Ui,
    th: &Theme,
    title: &str,
    subtitle: &str,
    body: impl FnOnce(&mut egui::Ui),
) {
    section_with_action(ui, th, title, subtitle, |_| {}, body);
}

/// [`section`] with a control in the heading row (e.g. "Add screen").
pub fn section_with_action(
    ui: &mut egui::Ui,
    th: &Theme,
    title: &str,
    subtitle: &str,
    action: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::NONE
        .fill(th.surface)
        .corner_radius(R_CARD)
        .stroke(egui::Stroke::new(1.0_f32, th.border))
        .inner_margin(egui::Margin::symmetric(SP_5 as i8, SP_4 as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(title)
                            .size(typography::SECTION)
                            .color(th.text),
                    );
                    if !subtitle.is_empty() {
                        ui.label(
                            egui::RichText::new(subtitle)
                                .size(typography::CAPTION)
                                .color(th.muted),
                        );
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    action(ui);
                });
            });
            ui.add_space(SP_4);
            body(ui);
        });
    ui.add_space(SP_3);
}

/// A hairline that spans the content width, for separating rows inside a card.
pub fn divider(ui: &mut egui::Ui, th: &Theme) {
    ui.add_space(SP_3);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        (1.0_f32, th.border_soft),
    );
    ui.add_space(SP_3);
}

// ---------------------------------------------------------------------------------------------
// Form controls
// ---------------------------------------------------------------------------------------------

/// A labelled form row: fixed-width label column on the left, control on the right.
///
/// The label column is what stops forms from looking like a stack of full-width banners, and what
/// makes several rows scan as one table.
pub fn form_row(
    ui: &mut egui::Ui,
    th: &Theme,
    label: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_4;
        ui.allocate_ui_with_layout(
            vec2(LABEL_W, CTRL_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label).size(typography::LABEL).color(th.muted),
                    )
                    .truncate(),
                );
            },
        );
        control(ui);
    });
    ui.add_space(SP_2);
}

/// A single-line text field at a fixed width.
pub fn text_field(
    ui: &mut egui::Ui,
    value: &mut String,
    width: f32,
    monospace: bool,
) -> egui::Response {
    let mut edit = egui::TextEdit::singleline(value).desired_width(width);
    if monospace {
        edit = edit.font(egui::TextStyle::Monospace);
    }
    ui.add_sized([width, CTRL_H], edit)
}

/// Helper text under a control.
pub fn hint(ui: &mut egui::Ui, th: &Theme, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(typography::CAPTION)
            .color(th.faint),
    );
}

// ---------------------------------------------------------------------------------------------
// Small pieces
// ---------------------------------------------------------------------------------------------

/// A pill badge, e.g. "在线" or a peer count.
pub fn badge(ui: &mut egui::Ui, th: &Theme, color: Color32, label: &str) {
    let font = FontId::proportional(typography::CAPTION);
    let w = text_width(ui, label, &font) + 14.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(9), th.soft_for(color));
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, label, font, color);
}

/// A coloured dot followed by a label — the connection state atom.
pub fn dot_label(ui: &mut egui::Ui, th: &Theme, color: Color32, label: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 18.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 3.5, color);
        ui.label(
            egui::RichText::new(label)
                .size(typography::LABEL)
                .color(th.text),
        );
    });
}

/// One cell of a stat row.
pub struct Stat<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub foot: &'a str,
    pub color: Color32,
}

/// A row of equal-width statistics, separated by hairlines.
///
/// Painted rather than laid out with widgets so the columns stay exactly equal and the dividers
/// land on the boundaries. Values are measured and ellipsised, because a stat value is generated
/// (a machine name, a control-plane phrase) and has no length bound.
pub fn stat_row(ui: &mut egui::Ui, th: &Theme, stats: &[Stat]) {
    let n = stats.len().max(1);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::hover());
    let cell_w = rect.width() / n as f32;
    let label_font = FontId::proportional(typography::CAPTION);
    let value_font = FontId::proportional(typography::METRIC);
    let foot_font = FontId::proportional(typography::CAPTION);

    for (i, s) in stats.iter().enumerate() {
        let cell = Rect::from_min_size(
            pos2(rect.min.x + cell_w * i as f32, rect.min.y),
            vec2(cell_w, rect.height()),
        );
        let inner = Rect::from_min_max(
            pos2(cell.min.x + if i == 0 { 0.0 } else { SP_5 }, cell.min.y),
            pos2(cell.max.x - SP_4, cell.max.y),
        );
        if i > 0 {
            ui.painter().line_segment(
                [
                    pos2(cell.min.x, cell.min.y + 4.0),
                    pos2(cell.min.x, cell.max.y - 4.0),
                ],
                (1.0_f32, th.border_soft),
            );
        }
        ui.painter().text(
            pos2(inner.min.x, inner.min.y + 2.0),
            Align2::LEFT_TOP,
            s.label,
            label_font.clone(),
            th.muted,
        );
        let value = ellipsize(ui, s.value, &value_font, inner.width());
        ui.painter().text(
            pos2(inner.min.x, inner.min.y + 20.0),
            Align2::LEFT_TOP,
            value,
            value_font.clone(),
            s.color,
        );
        if !s.foot.is_empty() {
            let foot = ellipsize(ui, s.foot, &foot_font, inner.width());
            ui.painter().text(
                pos2(inner.min.x, inner.min.y + 44.0),
                Align2::LEFT_TOP,
                foot,
                foot_font.clone(),
                th.faint,
            );
        }
    }
}

/// A row in a list of machines / peers: icon, name, metadata, trailing controls.
pub fn list_row(
    ui: &mut egui::Ui,
    th: &Theme,
    icon: Icon,
    tint: Color32,
    title: &str,
    meta: &str,
    trailing: impl FnOnce(&mut egui::Ui),
) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), 44.0),
        Sense::hover(),
    );
    // Icon chip.
    let chip = Rect::from_min_size(
        pos2(rect.min.x + 2.0, rect.center().y - 14.0),
        vec2(28.0, 28.0),
    );
    ui.painter()
        .rect_filled(chip, egui::CornerRadius::same(R_CTRL), th.soft_for(tint));
    draw_icon(
        ui.painter(),
        Rect::from_center_size(chip.center(), vec2(15.0, 15.0)),
        icon,
        tint,
    );

    // Text column — clipped to whatever the trailing controls leave behind.
    let text_left = chip.max.x + SP_3;
    let mut trailing_w = 0.0;
    ui.scope(|ui| {
        ui.set_width(0.0);
        // Measure afterwards is not possible, so reserve a generous gutter instead.
        trailing_w = 0.0;
    });
    let _ = trailing_w;
    ui.painter().with_clip_rect(rect).text(
        pos2(text_left, rect.center().y - 9.0),
        Align2::LEFT_TOP,
        ellipsize(ui, title, &FontId::proportional(typography::BODY), rect.width() * 0.5),
        FontId::proportional(typography::BODY),
        th.text,
    );
    if !meta.is_empty() {
        ui.painter().with_clip_rect(rect).text(
            pos2(text_left, rect.center().y + 6.0),
            Align2::LEFT_TOP,
            ellipsize(ui, meta, &FontId::monospace(typography::CAPTION), rect.width() * 0.5),
            FontId::monospace(typography::CAPTION),
            th.muted,
        );
    }

    // Trailing controls are laid out in the row's own ui, right-aligned.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(rect.max.x - 220.0, rect.min.y),
                rect.max,
            ))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    trailing(&mut child);
    ui.add_space(SP_2);
}

/// One line of the activity feed: status dot, message, relative time.
///
/// Laid out by hand rather than with nested layouts. A `right_to_left` group containing a
/// truncating label gives the label whatever width it happens to want, which pushes the message
/// up against the timestamp instead of letting it start at the left margin. Measuring the
/// timestamp first and ellipsising the message into the space that remains keeps the column of
/// messages left-aligned down the whole feed.
pub fn activity_row(ui: &mut egui::Ui, th: &Theme, dot: Color32, age: &str, msg: &str) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
    let age_font = FontId::proportional(typography::CAPTION);
    let age_w = text_width(ui, age, &age_font);

    ui.painter()
        .circle_filled(pos2(rect.min.x + 4.0, rect.center().y), 3.0, dot);
    ui.painter().text(
        pos2(rect.max.x, rect.center().y),
        Align2::RIGHT_CENTER,
        age,
        age_font,
        th.faint,
    );

    let left = rect.min.x + 16.0;
    let avail = (rect.max.x - age_w - 12.0 - left).max(24.0);
    let font = FontId::proportional(typography::LABEL);
    let text = ellipsize(ui, msg, &font, avail);
    ui.painter().text(
        pos2(left, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        th.text,
    );
    ui.add_space(SP_2);
}

/// An empty-state line.
pub fn empty_note(ui: &mut egui::Ui, th: &Theme, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(typography::LABEL)
            .color(th.muted),
    );
}

// ---------------------------------------------------------------------------------------------
// Page chrome
// ---------------------------------------------------------------------------------------------

/// The heading at the top of every content page.
pub fn page_header(ui: &mut egui::Ui, th: &Theme, title: &str, subtitle: &str) {
    ui.label(
        egui::RichText::new(title)
            .size(typography::TITLE)
            .color(th.text),
    );
    if !subtitle.is_empty() {
        ui.add_space(SP_1);
        ui.label(
            egui::RichText::new(subtitle)
                .size(typography::LABEL)
                .color(th.muted),
        );
    }
    ui.add_space(SP_5);
}

/// The wordmark at the top of the rail, with the running version.
///
/// The version is deliberately visible: it is the only way for a user to confirm that the build
/// they just installed is the one they think it is, which comes up every time behaviour is
/// reported as unchanged after a fix.
pub fn brand(ui: &mut egui::Ui, th: &Theme) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("MouseShare")
                .size(14.0)
                .color(th.text),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                    .size(typography::CAPTION)
                    .color(th.faint),
            );
        });
    });
    ui.add_space(SP_4);
}

/// A navigation group heading.
pub fn nav_group(ui: &mut egui::Ui, th: &Theme, text: &str) {
    ui.add_space(SP_4);
    ui.label(
        egui::RichText::new(text)
            .size(typography::CAPTION)
            .color(th.faint),
    );
    ui.add_space(SP_1);
}

/// A sidebar navigation row. Returns `true` when clicked.
pub fn nav_item(
    ui: &mut egui::Ui,
    th: &Theme,
    selected: bool,
    label: &str,
    icon: Icon,
    badge_count: Option<&(String, Color32)>,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_CTRL), th.selected_fill);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_CTRL), th.nav_hover);
    }

    let tint = if selected { th.accent } else { th.muted };
    draw_icon(
        ui.painter(),
        Rect::from_min_size(
            pos2(rect.min.x + 8.0, rect.center().y - 8.0),
            vec2(16.0, 16.0),
        ),
        icon,
        tint,
    );

    let mut right = 0.0;
    if let Some((text, col)) = badge_count {
        let w = 10.0 + text.chars().count() as f32 * 6.4;
        let pill = Rect::from_min_size(
            pos2(rect.max.x - 8.0 - w, rect.center().y - 8.0),
            vec2(w, 16.0),
        );
        ui.painter()
            .rect_filled(pill, egui::CornerRadius::same(8), *col);
        ui.painter().text(
            pill.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(10.0),
            Color32::WHITE,
        );
        right = w + 12.0;
    }

    let font = FontId::proportional(typography::BODY);
    let avail = rect.width() - 32.0 - right;
    ui.painter().text(
        pos2(rect.min.x + 32.0, rect.center().y),
        Align2::LEFT_CENTER,
        ellipsize(ui, label, &font, avail),
        font,
        if selected { th.accent } else { th.text },
    );

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// The language switch: a two-segment control showing both languages, the active one filled.
///
/// A segmented control rather than a toggle button because it states the *available* options and
/// the *current* one at the same time. A single button labelled with the other language ("English"
/// while in Chinese) reads as ambiguous — it is never clear whether it names the current state or
/// the action.
pub fn lang_segmented(ui: &mut egui::Ui, th: &Theme, lang: crate::i18n::Lang) -> Option<crate::i18n::Lang> {
    use crate::i18n::Lang;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), CTRL_H), Sense::hover());
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(R_CTRL),
        th.surface_alt,
    );

    let seg_w = rect.width() / 2.0;
    let font = FontId::proportional(typography::LABEL);
    let mut picked = None;
    for (i, (code, text)) in [("zh", "中文"), ("en", "English")].iter().enumerate() {
        let seg = Rect::from_min_size(
            pos2(rect.min.x + seg_w * i as f32 + 2.0, rect.min.y + 2.0),
            vec2(seg_w - 4.0, rect.height() - 4.0),
        );
        let active = code.eq_ignore_ascii_case(lang.code());
        let resp = ui.interact(seg, ui.id().with(("lang-seg", i)), Sense::click());
        if active {
            ui.painter()
                .rect_filled(seg, egui::CornerRadius::same(R_CTRL - 1), th.surface);
            ui.painter().rect_stroke(
                seg,
                egui::CornerRadius::same(R_CTRL - 1),
                (1.0_f32, th.border),
                egui::StrokeKind::Inside,
            );
        }
        ui.painter().text(
            seg.center(),
            Align2::CENTER_CENTER,
            *text,
            font.clone(),
            if active {
                th.text
            } else if resp.hovered() {
                th.text
            } else {
                th.muted
            },
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() && !active {
            picked = Some(if *code == "zh" { Lang::Zh } else { Lang::En });
        }
    }
    picked
}

// ---------------------------------------------------------------------------------------------
// Choice cards
// ---------------------------------------------------------------------------------------------

/// A large selectable card, used for the primary / secondary role choice.
///
/// A radio group with room for a sentence of explanation. Radio buttons alone would fit the label
/// but not the "which one do I pick" text, which is the only thing that makes this page usable on
/// first run.
pub fn choice_card(
    ui: &mut egui::Ui,
    th: &Theme,
    width: f32,
    selected: bool,
    title: &str,
    desc: &str,
    icon: Icon,
) -> bool {
    const H: f32 = 94.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, H), Sense::click());
    let hovered = resp.hovered();

    let fill = if selected {
        th.selected_fill
    } else if hovered {
        th.surface_alt
    } else {
        th.recessed
    };
    let stroke = if selected {
        (1.5_f32, th.accent)
    } else if hovered {
        (1.0_f32, th.border)
    } else {
        (1.0_f32, Color32::TRANSPARENT)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(R_CARD), fill);
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(R_CARD),
        stroke,
        egui::StrokeKind::Inside,
    );

    let tint = if selected { th.accent } else { th.muted };
    draw_icon(
        ui.painter(),
        Rect::from_min_size(
            pos2(rect.min.x + SP_4, rect.min.y + SP_4),
            vec2(18.0, 18.0),
        ),
        icon,
        tint,
    );

    let text_left = rect.min.x + SP_4 + 18.0 + SP_3;
    let text_w = (rect.max.x - SP_4 - text_left).max(40.0);
    ui.painter().text(
        pos2(text_left, rect.min.y + SP_4 + 1.0),
        Align2::LEFT_TOP,
        ellipsize(ui, title, &FontId::proportional(typography::SECTION), text_w),
        FontId::proportional(typography::SECTION),
        if selected { th.accent } else { th.text },
    );

    let desc_font = FontId::proportional(typography::LABEL);
    let mut y = rect.min.y + SP_4 + 24.0;
    for line in wrap_lines(ui, desc, &desc_font, text_w) {
        if y + 16.0 > rect.max.y - SP_2 {
            break;
        }
        ui.painter().text(
            pos2(text_left, y),
            Align2::LEFT_TOP,
            line,
            desc_font.clone(),
            th.muted,
        );
        y += 16.0;
    }

    // Radio indicator, top-right.
    let c = pos2(rect.max.x - SP_4 - 7.0, rect.min.y + SP_4 + 7.0);
    ui.painter()
        .circle_stroke(c, 7.0, egui::Stroke::new(1.5_f32, if selected { th.accent } else { th.border }));
    if selected {
        ui.painter().circle_filled(c, 4.0, th.accent);
    }

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}
