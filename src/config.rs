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
                    ox: 0,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                }],
            },
            primary_name: host,
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
        if let Ok(c) = serde_json::from_str::<Config>(&s) {
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
