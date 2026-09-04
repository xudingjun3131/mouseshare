//! File-based diagnostic logging for the cursor hand-off.
//!
//! `env_logger` writes to stderr, which is invisible when the app is launched from Finder —
//! so when crossing misbehaves there was historically *no way to see why*. This module
//! appends the interesting control-plane decisions (edge pins, hand-offs, returns, the
//! layout/bbox at startup, sampled cursor positions) to `mouseshare.log` next to the
//! config file. The log is deliberately small: only decision points and throttled samples
//! are written, never the raw event flood.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static DIAG_LOCK: Mutex<()> = Mutex::new(());

/// Append one line to the diagnostic log. Never panics: logging must not take the app down.
pub fn log(msg: &str) {
    let _guard = DIAG_LOCK.lock();
    let path = crate::config::config_dir().join("mouseshare.log");
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = writeln!(f, "{} {}", now, msg);
}

/// Where the log lives (shown in the GUI so users can find and paste it).
pub fn log_path() -> std::path::PathBuf {
    crate::config::config_dir().join("mouseshare.log")
}
