//! File-based diagnostic logging for the cursor hand-off.
//!
//! `env_logger` writes to stderr, which is invisible when the app is launched from Finder —
//! so when crossing misbehaves there was historically *no way to see why*. This module
//! appends the interesting control-plane decisions (edge pins, hand-offs, returns, the
//! layout/bbox at startup, sampled cursor positions) to `mouseshare.log` next to the
//! config file. The log is deliberately small: only decision points and throttled samples
//! are written, never the raw event flood.

// These are only used by the real (non-test) diagnostic writer.
#[cfg(not(test))]
use std::fs::OpenOptions;
#[cfg(not(test))]
use std::io::Write;
#[cfg(not(test))]
use std::sync::Mutex;
#[cfg(not(test))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(test))]
static DIAG_LOCK: Mutex<()> = Mutex::new(());

/// Append one line to the diagnostic log. Never panics: logging must not take the app down.
#[cfg(not(test))]
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

/// Under test we never want to append to the user's real diagnostic log (and there is no GUI
/// session to diagnose anyway), so `log` is a no-op.
#[cfg(test)]
pub fn log(_msg: &str) {}

/// Where the log lives (shown in the GUI so users can find and paste it).
pub fn log_path() -> std::path::PathBuf {
    crate::config::config_dir().join("mouseshare.log")
}
