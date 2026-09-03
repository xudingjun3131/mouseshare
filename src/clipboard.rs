//! Clipboard monitoring + setting, with loop suppression so a remote update isn't echoed back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Set the local clipboard. Used both by secondaries (remote -> local) and the primary
/// (when it wants to mirror a peer's clipboard locally).
pub fn set_clipboard(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

/// Monitor the local clipboard. Whenever it changes to a value different from what we last
/// saw (local or remote), call `on_change` with the new text. `last_seen` is shared so that a
/// value we just set remotely is not treated as a fresh local change.
pub fn start_monitor(last_seen: Arc<Mutex<String>>, on_change: impl Fn(String) + Send + 'static) {
    std::thread::spawn(move || {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("clipboard unavailable: {}", e);
                return;
            }
        };
        let mut last = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(t) = cb.get_text() {
                if t != last {
                    last = t.clone();
                    let seen = last_seen.lock().unwrap().clone();
                    if t != seen {
                        *last_seen.lock().unwrap() = t.clone();
                        on_change(t);
                    }
                }
            }
        }
    });
}
