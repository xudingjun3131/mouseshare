//! Persistent configuration (name, role, network address, screen layout).

use crate::layout::{Layout, Screen};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// This machine's unique name (used as the screen id in the layout).
    pub name: String,
    /// "primary" = machine with the physical mouse/keyboard (the hub/server).
    /// "secondary" = receives input from the primary (the client).
    pub mode: String,
    /// For secondaries: `host:port` of the primary.
    pub server_addr: String,
    /// TCP port the primary listens on.
    pub port: u16,
    /// The full multi-machine screen layout (edited in the GUI, shared by all machines).
    pub layout: Layout,
    /// Name of the machine that acts as primary (must equal that machine's `name`).
    pub primary_name: String,
    /// UI language: "zh" (default) or "en".
    #[serde(default = "default_lang")]
    pub lang: String,
    /// Extra multiplier applied on top of the **automatically derived** forwarded-delta ratio
    /// (see `control::motion_scale_ratio`). `1.0` = pure auto (the two machines' OS scale
    /// factors decide). Because a scale factor is only a proxy for real pixel density, the
    /// auto value can feel off on mismatched monitors — this lets the user trim it without
    /// a rebuild. Clamped to a sane range on use.
    #[serde(default = "default_motion_scale")]
    pub motion_scale: f32,
}

fn default_lang() -> String {
    "zh".to_string()
}

fn default_motion_scale() -> f32 {
    1.0
}

impl Default for Config {
    fn default() -> Self {
        let host = hostname();
        Config {
            name: host.clone(),
            mode: "primary".to_string(),
            server_addr: "192.168.1.100:49152".to_string(),
            port: 49152,
            layout: Layout {
                screens: vec![Screen {
                    name: host.clone(),
                    host: host.clone(),
                    ox: 0,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 1.0,
                }],
            },
            primary_name: host,
            lang: default_lang(),
            motion_scale: default_motion_scale(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(p) = directories::ProjectDirs::from("", "", "mouseshare") {
        let d = p.config_dir().to_path_buf();
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    PathBuf::from(".")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn load_config() -> Config {
    let p = config_path();
    if let Ok(s) = std::fs::read_to_string(&p) {
        if let Ok(mut c) = serde_json::from_str::<Config>(&s) {
            // A layout written before `Screen::host` existed carries an empty `host` on every
            // panel. Repair it here — the single place a layout can enter the process from disk —
            // so a multi-monitor peer cannot come back as duplicated panels (see
            // `Layout::normalize_hosts`).
            c.layout.normalize_hosts();
            return c;
        }
    }
    Config::default()
}

pub fn save_config(c: &Config) {
    let p = config_path();
    if let Ok(s) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(&p, s);
    }
}

#[allow(deprecated)]
fn hostname() -> String {
    whoami::hostname()
}
