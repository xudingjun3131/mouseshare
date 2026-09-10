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
use crate::i18n::{tr, Lang, Severity, Tr};
use crate::layout::Layout;
use crate::network::{connect_client, Net};
use crate::protocol::Message;
use crate::ui::{self, Btn, Icon, Theme};
use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Id, Rect, Sense};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
pub enum Page {
    Connection,
    Layout,
    Status,
    Discovered,
}

// ---- Theme, fonts and the widget vocabulary live in `crate::ui` -------------------------
//
// `app.rs` owns state and composes pages; it never hard-codes a colour or a pixel value.

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
    fn show_permission_dialog(&mut self, ctx: &egui::Context, t: Tr, theme: Theme) {
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
                    .fill(theme.surface)
                    .corner_radius(ui::R_CARD)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border))
                    .inner_margin(egui::Margin::symmetric(ui::SP_6 as i8, ui::SP_5 as i8)),
            )
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = ui::SP_2;
                    let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 10.0, theme.orange_soft);
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        "!",
                        FontId::proportional(13.0),
                        theme.orange,
                    );
                    ui.label(
                        egui::RichText::new(t.perm_title)
                            .size(ui::typography::SECTION)
                            .color(theme.text),
                    );
                });
                ui.add_space(ui::SP_3);
                ui.label(
                    egui::RichText::new(t.perm_body)
                        .size(ui::typography::BODY)
                        .color(theme.muted),
                );
                ui.add_space(ui::SP_5);

                // Two "open System Settings" buttons, then the primary re-check + dismiss row.
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = ui::SP_2;
                    if ui::button(ui, &theme, t.perm_open_input, Btn::Secondary) {
                        open_input = true;
                    }
                    if ui::button(ui, &theme, t.perm_open_accessibility, Btn::Secondary) {
                        open_accessibility = true;
                    }
                });
                ui.add_space(ui::SP_4);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = ui::SP_3;
                    if ui::button(ui, &theme, t.perm_recheck, Btn::Primary) {
                        recheck = true;
                    }
                    if ui::button(ui, &theme, t.perm_dismiss, Btn::Quiet) {
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

        let theme = Theme::from_ctx(ctx);

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
        if let Some(err) = self.startup_error.clone() {
            let is_secondary = self.config.mode == "secondary";
            egui::TopBottomPanel::top("startup_error")
                .frame(egui::Frame::NONE.fill(theme.banner_error).inner_margin(egui::Margin {
                    left: ui::CONTENT_MIN_PAD as i8,
                    right: ui::CONTENT_MIN_PAD as i8,
                    top: ui::TITLEBAR_CLEARANCE,
                    bottom: ui::SP_3 as i8,
                }))
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = ui::SP_2;
                        ui.label(
                            egui::RichText::new(t.err_title)
                                .size(ui::typography::BODY)
                                .color(theme.red),
                        );
                        ui.label(
                            egui::RichText::new(&err)
                                .size(ui::typography::BODY)
                                .color(theme.text),
                        );
                    });
                    ui.add_space(ui::SP_1);
                    ui.label(
                        egui::RichText::new(t.err_hint)
                            .size(ui::typography::CAPTION)
                            .color(theme.muted),
                    );
                    if is_secondary {
                        ui.add_space(ui::SP_2);
                        retry_clicked = ui::button(ui, &theme, t.retry_connect, Btn::Secondary);
                    }
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

        // ---- Navigation rail ----
        egui::SidePanel::left("nav")
            .exact_width(ui::SIDEBAR_W)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme.sidebar).inner_margin(egui::Margin {
                left: ui::SP_3 as i8,
                right: ui::SP_3 as i8,
                top: ui::TITLEBAR_CLEARANCE,
                bottom: ui::SP_3 as i8,
            }))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui::brand(ui, &theme);

                let peer_count = self.net.lock().unwrap().peer_count();
                let disc_count = self.discovered.lock().unwrap().len();

                ui::nav_group(ui, &theme, t.nav_group_config);
                if ui::nav_item(
                    ui,
                    &theme,
                    self.page == Page::Connection,
                    t.nav_connection,
                    Icon::Displays,
                    None,
                ) {
                    self.page = Page::Connection;
                }
                if ui::nav_item(
                    ui,
                    &theme,
                    self.page == Page::Layout,
                    t.nav_layout,
                    Icon::Monitor,
                    None,
                ) {
                    self.page = Page::Layout;
                }
                let status_badge = if peer_count > 0 {
                    Some((format!("{peer_count}"), theme.accent))
                } else {
                    None
                };
                if ui::nav_item(
                    ui,
                    &theme,
                    self.page == Page::Status,
                    t.nav_status,
                    Icon::Clock,
                    status_badge.as_ref(),
                ) {
                    self.page = Page::Status;
                }

                if self.config.mode == "secondary" {
                    ui::nav_group(ui, &theme, t.nav_group_network);
                    let disc_badge = if disc_count > 0 {
                        Some((format!("{disc_count}"), theme.green))
                    } else {
                        None
                    };
                    if ui::nav_item(
                        ui,
                        &theme,
                        self.page == Page::Discovered,
                        t.nav_discovered,
                        Icon::Link,
                        disc_badge.as_ref(),
                    ) {
                        self.page = Page::Discovered;
                    }
                }

                // Footer, pinned to the foot of the rail: connection state, then the language
                // switch, then Quit (bottom-up order, so Quit ends up lowest).
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = ui::SP_2;
                    if ui::nav_item(ui, &theme, false, t.exit_app, Icon::Power, None) {
                        if self.config.mode == "primary" {
                            self.config.layout = self.shared_layout.lock().unwrap().clone();
                            save_config(&self.config);
                        }
                        std::process::exit(0);
                    }
                    if let Some(next) = ui::lang_segmented(ui, &theme, self.lang) {
                        self.lang = next;
                        self.config.lang = next.code().to_string();
                        crate::i18n::set_lang(next);
                        save_config(&self.config);
                    }
                    let (dot, label) = match &*self.net.lock().unwrap() {
                        Net::Primary { .. } => (theme.accent, t.conn_primary),
                        Net::Secondary { .. } => (theme.green, t.conn_connected),
                        Net::Idle => (theme.orange, t.conn_idle),
                    };
                    ui::dot_label(ui, &theme, dot, label);
                });
            });

        // ---- Main content: whichever page the rail has selected ----
        // The column is centred and capped at `CONTENT_MAX_W`. Letting a two-field form stretch
        // across an ultrawide window is the fastest way for a desktop app to look improvised.
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme.window))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let side = ((ui.available_width() - ui::CONTENT_MAX_W) / 2.0)
                            .max(ui::CONTENT_MIN_PAD);
                        egui::Frame::NONE
                            .inner_margin(egui::Margin {
                                left: side as i8,
                                right: side as i8,
                                top: ui::SP_6 as i8,
                                bottom: ui::SP_8 as i8,
                            })
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                match self.page {
                                    Page::Connection => self.page_connection(ui, t, theme),
                                    Page::Layout => self.page_layout(ui, t, theme),
                                    Page::Status => self.page_status(ui, t, theme),
                                    Page::Discovered => self.page_discovered(ui, t, theme),
                                }
                            });
                    });
            });

        // ---- Transient toast: bottom-centre overlay ----
        if let Some((_, msg)) = &self.toast {
            let msg = msg.clone();
            egui::Area::new(egui::Id::new("toast"))
                .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -28.0))
                .order(egui::Order::Tooltip)
                .show(ctx, |ui| {
                    egui::Frame::NONE
                        .fill(theme.surface)
                        .corner_radius(ui::R_CARD)
                        .inner_margin(egui::Margin::symmetric(ui::SP_4 as i8, 10))
                        .stroke(egui::Stroke::new(1.0_f32, theme.border))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = ui::SP_2;
                                let (r, _) =
                                    ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                                ui.painter().circle_filled(r.center(), 4.0, theme.green);
                                ui.label(
                                    egui::RichText::new(&msg)
                                        .size(ui::typography::BODY)
                                        .color(theme.text),
                                );
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
    fn recent_activity(&mut self, theme: &Theme, max: usize) -> Vec<(String, String, Color32)> {
        let now = Instant::now();
        let stale = match self.activity_poll {
            Some(t0) => now.duration_since(t0) >= Duration::from_secs(2),
            None => true,
        };
        if stale {
            self.activity_cache = read_activity(self.lang, theme, max);
            self.activity_poll = Some(now);
        }
        self.activity_cache.clone()
    }

    /// Connection: role, role-specific networking, pointer trim, and this machine's screens.
    fn page_connection(&mut self, ui: &mut egui::Ui, t: Tr, theme: Theme) {
        ui::page_header(ui, &theme, t.page_connection, t.page_connection_sub);

        // ---- Role ----
        ui::section(ui, &theme, t.card_role, t.card_role_sub, |ui| {
            let gap = ui::SP_3;
            let w = ((ui.available_width() - gap) / 2.0).clamp(180.0, 360.0);
            let mut picked: Option<&'static str> = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                if ui::choice_card(
                    ui,
                    &theme,
                    w,
                    self.config.mode == "primary",
                    t.role_primary_card,
                    t.role_primary_desc,
                    Icon::Displays,
                ) {
                    picked = Some("primary");
                }
                if ui::choice_card(
                    ui,
                    &theme,
                    w,
                    self.config.mode == "secondary",
                    t.role_secondary_card,
                    t.role_secondary_desc,
                    Icon::Link,
                ) {
                    picked = Some("secondary");
                }
            });
            if let Some(p) = picked {
                self.config.mode = p.to_string();
            }
        });

        if self.config.mode == "secondary" {
            // ---- Joining an existing host ----
            let mut connect = false;
            ui::section(ui, &theme, t.card_secondary, t.card_secondary_sub, |ui| {
                ui::form_row(ui, &theme, t.machine_name, |ui| {
                    ui::text_field(ui, &mut self.config.name, ui::FIELD_W, false);
                    ui.add_space(ui::SP_3);
                    ui::hint(ui, &theme, t.machine_name_hint);
                });
                ui::form_row(ui, &theme, t.server_addr, |ui| {
                    ui::text_field(ui, &mut self.config.server_addr, ui::FIELD_W, true);
                });
                ui::form_row(ui, &theme, t.primary_name, |ui| {
                    ui::text_field(ui, &mut self.config.primary_name, ui::FIELD_W, false);
                });
                ui::divider(ui, &theme);
                connect = ui::button(ui, &theme, t.connect_host, Btn::Primary);
            });
            if connect {
                self.reconnect();
            }
        } else {
            // ---- Serving this machine ----
            let mut detect = false;
            let mut save = false;
            let mut copy = false;
            ui::section(ui, &theme, t.card_primary, t.card_primary_sub, |ui| {
                ui::form_row(ui, &theme, t.machine_name, |ui| {
                    ui::text_field(ui, &mut self.config.name, ui::FIELD_W, false);
                    ui.add_space(ui::SP_3);
                    ui::hint(ui, &theme, t.machine_name_hint);
                });
                ui::form_row(ui, &theme, t.listen_port, |ui| {
                    ui.add_sized(
                        [92.0, ui::CTRL_H],
                        egui::DragValue::new(&mut self.config.port).speed(1),
                    );
                    ui.add_space(ui::SP_3);
                    ui::hint(ui, &theme, t.port_hint);
                });
                ui::form_row(ui, &theme, t.address, |ui| {
                    ui::text_field(ui, &mut self.config.server_addr, ui::FIELD_W, true);
                    ui.add_space(ui::SP_2);
                    copy = ui::button(ui, &theme, t.copy, Btn::Quiet);
                });
                ui::divider(ui, &theme);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = ui::SP_2;
                    save = ui::button(ui, &theme, t.save, Btn::Primary);
                    detect = ui::button(ui, &theme, t.detect_ip, Btn::Secondary);
                });
            });
            if copy {
                let addr = self.config.server_addr.clone();
                clipboard::set_clipboard(&addr);
                self.show_toast(t.copied);
            }
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

            // ---- Pointer speed across the seam ----
            // The baseline ratio is derived per hand-off from both machines' scale factors; this
            // row is only a manual trim on top of it, plus a readout of the result.
            ui::section(ui, &theme, t.card_speed, t.card_speed_sub, |ui| {
                let before = self.config.motion_scale;
                ui::form_row(ui, &theme, t.speed_multiplier, |ui| {
                    ui.add_sized(
                        [92.0, ui::CTRL_H],
                        egui::DragValue::new(&mut self.config.motion_scale)
                            .speed(0.05)
                            .range(0.25..=4.0)
                            .suffix(" ×"),
                    );
                    ui.add_space(ui::SP_3);
                    ui::hint(ui, &theme, t.speed_hint);
                });
                if (self.config.motion_scale - before).abs() > f32::EPSILON {
                    crate::control::set_motion_scale(self.config.motion_scale);
                    save_config(&self.config);
                }
                let auto = {
                    let l = self.shared_layout.lock().unwrap();
                    l.screens
                        .iter()
                        .find(|s| !s.is_local)
                        .map(|s| crate::control::motion_scale_ratio(&l, &s.name, None))
                };
                ui::form_row(ui, &theme, t.speed_effective, |ui| {
                    let (txt, col) = match auto {
                        Some(r) => (format!("{r:.2} ×"), theme.accent),
                        None => (t.speed_no_peer.to_string(), theme.muted),
                    };
                    ui.label(
                        egui::RichText::new(txt)
                            .size(ui::typography::BODY)
                            .color(col),
                    );
                });
            });
        }

        // ---- This machine's screens ----
        let mut dup_idx: Option<usize> = None;
        let mut del_idx: Option<usize> = None;
        let mut add = false;
        ui::section_with_action(
            ui,
            &theme,
            t.card_screens,
            t.card_screens_sub,
            |ui| {
                add = ui::button(ui, &theme, t.add_screen, Btn::Secondary);
            },
            |ui| {
                let layout = self.shared_layout.lock().unwrap();
                if layout.screens.is_empty() {
                    ui::empty_note(ui, &theme, t.screens_empty);
                    return;
                }
                let removable = layout.screens.len() > 1;
                for (i, s) in layout.screens.iter().enumerate() {
                    let meta = screen_meta(s);
                    ui::list_row(
                        ui,
                        &theme,
                        Icon::Monitor,
                        if s.is_local { theme.accent } else { theme.muted },
                        &s.name,
                        &meta,
                        |ui| {
                            if ui::button(ui, &theme, t.dup, Btn::Quiet) {
                                dup_idx = Some(i);
                            }
                            if removable && ui::button(ui, &theme, t.del, Btn::Danger) {
                                del_idx = Some(i);
                            }
                        },
                    );
                }
            },
        );
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
                name: format!("machine-{n}"),
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
    fn page_layout(&mut self, ui: &mut egui::Ui, t: Tr, theme: Theme) {
        ui::page_header(ui, &theme, t.page_layout, t.page_layout_sub);

        ui::section(ui, &theme, t.card_canvas, t.card_canvas_sub, |ui| {
            // Scoped: `set_clip_rect` below must not clip the caption that follows the canvas.
            let mut changed = false;
            ui.scope(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 380.0), Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(ui::R_CARD), theme.recessed);
            ui::dot_grid(ui.painter(), rect, theme.grid);
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::same(ui::R_CARD),
                (1.0_f32, theme.border_soft),
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
            changed = draw_layout(ui, &mut layout, &self.config.name, t, theme, rect, cur);
            drop(layout);
            });
            // Persist drag repositioning immediately (primary only — it owns the layout and
            // broadcasts it to every secondary within 2 s).
            if changed && self.config.mode == "primary" {
                self.config.layout = self.shared_layout.lock().unwrap().clone();
                save_config(&self.config);
            }
            ui.add_space(ui::SP_3);
            ui::hint(ui, &theme, t.layout_tip);
        });

        ui::section(ui, &theme, t.card_clients, t.card_clients_sub, |ui| {
            let layout = self.shared_layout.lock().unwrap();
            if layout.screens.is_empty() {
                ui::empty_note(ui, &theme, t.screens_empty);
                return;
            }
            for s in layout.screens.iter() {
                let meta = screen_meta(s);
                let live = s.name == self.my_name || s.is_local;
                ui::list_row(
                    ui,
                    &theme,
                    Icon::Monitor,
                    if live { theme.green } else { theme.muted },
                    &s.name,
                    &meta,
                    |ui| {
                        ui::badge(
                            ui,
                            &theme,
                            if live { theme.green } else { theme.faint },
                            if live { t.online } else { t.offline },
                        );
                    },
                );
            }
        });
    }

    /// Status: live session stats, the control-plane state, and a readable event feed.
    fn page_status(&mut self, ui: &mut egui::Ui, t: Tr, theme: Theme) {
        ui::page_header(ui, &theme, t.page_status, t.page_status_sub);

        let peers = self.net.lock().unwrap().peer_count().to_string();
        let (conn_label, conn_color) = match &*self.net.lock().unwrap() {
            Net::Primary { .. } => (t.conn_primary, theme.accent),
            Net::Secondary { .. } => (t.conn_connected, theme.green),
            Net::Idle => (t.conn_idle, theme.orange),
        };
        // A stat value gets one line; the explanation belongs in the tile's footnote. Passing
        // the whole sentence as the value is what truncated it mid-word in the previous layout.
        let (ctrl_value, ctrl_foot) = if self.config.mode == "primary" {
            let c = self.ctrl.lock().unwrap();
            match &c.remote {
                Some(r) => (r.name.clone(), t.ctrl_remote.replace("{}", &r.name)),
                None => (t.ctrl_local_short.to_string(), t.ctrl_local.to_string()),
            }
        } else {
            (t.ctrl_local_short.to_string(), t.ctrl_local.to_string())
        };

        ui::section(ui, &theme, t.card_stats, "", |ui| {
            ui::stat_row(
                ui,
                &theme,
                &[
                    ui::Stat {
                        label: t.stat_peers,
                        value: &peers,
                        foot: t.stat_peers_foot,
                        color: theme.text,
                    },
                    ui::Stat {
                        label: t.stat_conn,
                        value: conn_label,
                        foot: "",
                        color: conn_color,
                    },
                    ui::Stat {
                        label: t.stat_ctrl,
                        value: &ctrl_value,
                        foot: &ctrl_foot,
                        color: theme.text,
                    },
                ],
            );
            ui::divider(ui, &theme);
            ui::form_row(ui, &theme, t.local_name, |ui| {
                ui.label(
                    egui::RichText::new(&self.my_name)
                        .size(ui::typography::BODY)
                        .color(theme.text),
                );
            });
            ui.label(
                egui::RichText::new(t.hotkey_hint)
                    .size(ui::typography::LABEL)
                    .color(theme.muted),
            );
            ui.add_space(ui::SP_1);
            ui.label(
                egui::RichText::new(t.background_hint)
                    .size(ui::typography::LABEL)
                    .color(theme.muted),
            );
            if self.config.mode == "primary" {
                ui.add_space(ui::SP_1);
                ui.label(
                    egui::RichText::new(format!(
                        "{} {}",
                        t.diag_hint,
                        crate::diag::log_path().display()
                    ))
                    .size(ui::typography::CAPTION)
                    .color(theme.faint),
                );
            }
        });

        ui::section(ui, &theme, t.card_activity, t.card_activity_sub, |ui| {
            let events = self.recent_activity(&theme, 8);
            if events.is_empty() {
                ui::empty_note(ui, &theme, t.activity_empty);
                return;
            }
            for (age, msg, dot) in events {
                ui::activity_row(ui, &theme, dot, &age, &msg);
            }
        });

        let mut reconnect = false;
        if self.config.mode == "secondary" && matches!(&*self.net.lock().unwrap(), Net::Idle) {
            ui::section(ui, &theme, t.card_network, t.card_network_sub, |ui| {
                reconnect = ui::button(ui, &theme, t.reconnect_host, Btn::Primary);
            });
        }
        if reconnect {
            self.reconnect();
        }
    }

    /// Discovered hosts on the LAN (secondary only) — one click to connect.
    fn page_discovered(&mut self, ui: &mut egui::Ui, t: Tr, theme: Theme) {
        ui::page_header(ui, &theme, t.page_discovered, t.page_discovered_sub);

        let list = self.discovered.lock().unwrap().clone();
        let mut pick: Option<String> = None;
        ui::section(ui, &theme, t.card_discovered, t.card_discovered_sub, |ui| {
            if list.is_empty() {
                ui::empty_note(ui, &theme, t.discovered_empty);
                return;
            }
            for d in &list {
                let addr = d.addr();
                ui::list_row(
                    ui,
                    &theme,
                    Icon::Link,
                    theme.green,
                    &d.name,
                    &addr,
                    |ui| {
                        if ui::button(ui, &theme, t.discovered_connect, Btn::Secondary) {
                            pick = Some(addr.clone());
                        }
                    },
                );
            }
        });
        if let Some(addr) = pick {
            self.config.server_addr = addr.clone();
            self.connect_to(addr);
        }
    }
}

/// "1920 × 1080 · @2x", or just the resolution when the screen is not scaled.
fn screen_meta(s: &crate::layout::Screen) -> String {
    if s.physical_size() != (s.w, s.h) {
        format!("{} × {} · @{:.0}x", s.w, s.h, s.scale)
    } else {
        format!("{} × {}", s.w, s.h)
    }
}
/// Read the tail of the real diagnostic log and translate it into activity-feed entries, oldest
/// first, as `(age-label, message, dot-colour)`.
///
/// The log is a debugging artefact (see `crate::i18n::tr_event`), so most lines never reach the
/// feed. Distinct messages win over repeats: the log announces routine events over and over —
/// every mouse hand-off, every capture restart — and eight identical rows tell the user nothing,
/// so a message already shown in this window is skipped.
///
/// Called at most every 2 s via `recent_activity`.
fn read_activity(lang: Lang, theme: &Theme, max: usize) -> Vec<(String, String, Color32)> {
    let Ok(text) = std::fs::read_to_string(crate::diag::log_path()) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut out: Vec<(String, String, Color32)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in text.lines().rev() {
        let Some((ts, msg)) = line.split_once(' ') else {
            continue;
        };
        let Ok(ms) = ts.parse::<u64>() else { continue };
        let Some((text, sev)) = crate::i18n::tr_event(lang, msg) else {
            continue;
        };
        if seen.iter().any(|s| s == &text) {
            continue;
        }
        seen.push(text.clone());

        let secs = now.saturating_sub(ms) / 1000;
        let age = if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h", secs / 3600)
        };
        let dot = match sev {
            Severity::Ok => theme.green,
            Severity::Info => theme.accent,
            Severity::Warn => theme.orange,
            Severity::Error => theme.red,
        };
        out.push((age, text, dot));
        if out.len() >= max {
            break;
        }
    }
    out.reverse();
    out
}

/// Draw the virtual desktop. Returns `true` when the layout was changed by dragging, so the
/// caller can persist it.
fn draw_layout(
    ui: &mut egui::Ui,
    layout: &mut Layout,
    my_name: &str,
    t: Tr,
    theme: Theme,
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
    ui::dot_grid(ui.painter(), canvas_rect, theme.grid);

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

        let (top, bottom) = ui::tile_colors(is_primary, is_me);
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
        ui::soft_shadow(
            ui.painter(),
            rect,
            16.0,
            theme.shadow,
            if hover { 1.3 } else { 1.0 },
        );
        paint_tile(
            ui,
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
        let (top, bottom) = ui::tile_colors(is_primary, is_me);
        ui::soft_shadow(ui.painter(), rect, 16.0, theme.shadow, 2.0);
        paint_tile(
            ui,
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

    let _ = t;
    changed
}

/// Paint one screen tile as a **display**: a darker bezel, the screen surface inset inside it,
/// and the machine name / resolution captioned over a scrim at the bottom.
///
/// An earlier version filled the whole tile with a saturated gradient and drew two pale bars
/// across the top as a stand-in for a menu bar. At any real tile size that read as a progress
/// bar rather than a monitor, so the bars are gone and the shape of the tile does the work.
///
/// `top`/`bottom` colour the screen surface; the accent is reserved for "this machine".
///
/// `logical` is the screen size in OS logical points (the same space used for layout and
/// crossing), so canvas labels match the connected-clients list and macOS System Settings.
#[allow(clippy::too_many_arguments)]
fn paint_tile(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    name: &str,
    logical: (u32, u32),
    scale: f32,
    _is_primary: bool,
    is_me: bool,
    hover: bool,
    theme: Theme,
    top: Color32,
    bottom: Color32,
    me_label: &str,
) {
    const R: u8 = 10;
    if rect.width() < 18.0 || rect.height() < 18.0 {
        return;
    }

    let bezel_top = ui::mix(top, Color32::BLACK, 0.30);
    let bezel_bottom = ui::mix(bottom, Color32::BLACK, 0.30);
    ui::fill_gradient(painter, rect, R as f32, bezel_top, bezel_bottom);

    let screen = rect.shrink(3.0);
    if screen.width() < 12.0 || screen.height() < 12.0 {
        return;
    }
    let cp = painter.with_clip_rect(rect);
    ui::fill_gradient(&cp, screen, (R - 2) as f32, top, bottom);

    // Scrim, so the caption stays readable over any screen colour.
    let scrim_h = (screen.height() * 0.62).clamp(20.0, 54.0);
    let steps = 14;
    let band_h = scrim_h / steps as f32;
    for i in 0..steps {
        let t = i as f32 / (steps - 1) as f32;
        let band = Rect::from_min_size(
            pos2(screen.min.x, screen.max.y - scrim_h + i as f32 * band_h),
            vec2(screen.width(), band_h + 0.5),
        );
        cp.rect_filled(band, 0.0, Color32::from_black_alpha((125.0 * t * t) as u8));
    }

    let pad = 10.0;
    let text_w = (screen.width() - pad * 2.0).max(20.0);
    let res = if scale != 1.0 {
        format!("{} × {} · @{:.0}x", logical.0, logical.1, scale)
    } else {
        format!("{} × {}", logical.0, logical.1)
    };
    if screen.height() > 34.0 {
        let res_font = FontId::monospace(10.5);
        cp.text(
            pos2(screen.min.x + pad, screen.max.y - pad),
            Align2::LEFT_BOTTOM,
            ui::ellipsize(ui, &res, &res_font, text_w),
            res_font,
            Color32::from_white_alpha(160),
        );
        if screen.height() > 54.0 {
            let name_font = FontId::proportional(12.5);
            cp.text(
                pos2(screen.min.x + pad, screen.max.y - pad - 15.0),
                Align2::LEFT_BOTTOM,
                ui::ellipsize(ui, name, &name_font, text_w),
                name_font,
                Color32::WHITE,
            );
        }
    }

    // Border: accent while this machine's screen is hovered or is the local one; hairline otherwise.
    let (sw, sc) = if hover {
        (2.0_f32, theme.accent)
    } else if is_me {
        (1.5_f32, theme.accent)
    } else {
        (1.0_f32, theme.border)
    };
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(R),
        (sw, sc),
        egui::StrokeKind::Inside,
    );

    // "This machine" badge, top-left, inside the bezel so it never fights the caption.
    if is_me && rect.width() > 92.0 && rect.height() > 48.0 {
        let font = FontId::proportional(10.0);
        let w = ui::measure(ui, me_label, &font) + 16.0;
        let pill = Rect::from_min_size(pos2(rect.min.x + 9.0, rect.min.y + 9.0), vec2(w, 18.0));
        painter.rect_filled(pill, egui::CornerRadius::same(9), Color32::from_white_alpha(238));
        painter.text(
            pill.center(),
            Align2::CENTER_CENTER,
            me_label,
            font,
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
    theme: Theme,
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
