//! Windows system-tray icon (notification area).
//!
//! The app keeps running in the background when its window is closed/minimised — that is the
//! point of a sharing tool — but without a tray icon Windows had no visible handle on the
//! process: no way to bring the window back and no way to quit without Task Manager. This
//! module adds a tray icon with a two-entry menu: show the main window, quit the app.
//!
//! The tray must be created on the main thread (its hidden window receives its messages via
//! the same message pump winit runs later). Menu events arrive on a global channel and are
//! consumed by a dedicated thread; showing the window goes through the stored `egui::Context`
//! (viewport commands are applied on the next frame, woken via `request_repaint`).

#![cfg(target_os = "windows")]

use std::sync::OnceLock;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::TrayIconBuilder;

/// The egui context, stored on the first frame so the tray thread can show the window.
static GUI_CTX: OnceLock<eframe::egui::Context> = OnceLock::new();

const ID_SHOW: &str = "mouseshare-show";
const ID_QUIT: &str = "mouseshare-quit";

/// Called from the egui update loop (first frame) to register the context.
pub fn register_ctx(ctx: &eframe::egui::Context) {
    let _ = GUI_CTX.set(ctx.clone());
}

/// Build the tray icon and start the menu-event thread. `lang` picks the menu strings.
pub fn init(lang: crate::i18n::Lang) {
    let t = crate::i18n::tr(lang);
    let show_item = MenuItem::with_id(ID_SHOW, t.tray_show, true, None);
    let quit_item = MenuItem::with_id(ID_QUIT, t.exit_app, true, None);
    let mut menu = Menu::new();
    if menu.append(&show_item).is_err() || menu.append(&quit_item).is_err() {
        log::warn!("tray menu build failed");
        return;
    }

    // Reuse the bundled logo PNG: decode via eframe's icon loader (already compiled in),
    // then hand the raw RGBA pixels to the tray icon.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../resources/mouse-logo.png"))
        .ok()
        .and_then(|d| tray_icon::Icon::from_rgba(d.rgba, d.width, d.height).ok());

    let mut builder = TrayIconBuilder::new()
        .with_id("mouseshare-tray")
        .with_menu(Box::new(menu))
        .with_tooltip("MouseShare")
        .with_menu_on_left_click(true);
    if let Some(icon) = icon {
        builder = builder.with_icon(icon);
    }
    match builder.build() {
        // Intentionally leaked: the tray must live for the whole process lifetime, and
        // TrayIcon is !Send so it cannot be parked in a static for the event thread.
        Ok(tray) => std::mem::forget(tray),
        Err(e) => {
            log::warn!("tray icon creation failed: {e}");
            return;
        }
    }

    // Consume menu events off-thread so they work even while the window is minimised and
    // egui is not producing frames.
    std::thread::spawn(|| loop {
        if let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id().as_ref() {
                ID_SHOW => {
                    if let Some(ctx) = GUI_CTX.get() {
                        use eframe::egui::ViewportCommand;
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(ViewportCommand::Focus);
                        ctx.request_repaint();
                    }
                }
                ID_QUIT => {
                    log::info!("quit requested from tray");
                    std::process::exit(0);
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    });
}
