//! Clipboard monitoring + setting, with loop suppression so a remote update isn't echoed back.
//!
//! Two kinds of clipboard travel between machines:
//!
//! * **text** — a single `Message::Clipboard` frame, handled by `arboard`.
//! * **files** — streamed by [`crate::transfer`]; detected and written through
//!   [`crate::clipfile`] (native pasteboard access on macOS / Windows).
//!
//! ## Loop suppression
//!
//! Every machine both *sends* and *receives* clipboard content, so a naive "on change, broadcast"
//! monitor echoes forever: A copies → B sets it locally → B's monitor sees the change → B
//! broadcasts → A sets it → … The fix is [`ClipState`]: a shared record of the last value **we
//! put on the clipboard ourselves** (locally or from a remote). The monitor only broadcasts a
//! value that differs from it, and every write — local or remote — updates it.

use crate::clipfile;
use crate::network::Net;
use crate::protocol::Message;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the local clipboard is polled. 200 ms is well below the time a user takes to
/// switch windows after a copy, and cheap enough (one pasteboard read) to run forever.
const POLL_MS: u64 = 200;

/// The last value this process put on the local clipboard, so the monitor can tell a *fresh*
/// copy from our own write.
#[derive(Debug, Default)]
pub struct ClipState {
    pub text: Option<String>,
    pub files: Vec<PathBuf>,
}

/// Set the local clipboard text. Used both by secondaries (remote -> local) and the primary
/// (when it wants to mirror a peer's clipboard locally).
///
/// Returns `true` when the value actually changed, so callers can decide whether to relay.
pub fn set_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text.to_string()).is_ok(),
        Err(e) => {
            log::debug!("clipboard unavailable: {}", e);
            false
        }
    }
}

/// Record a remote value as "already seen" *before* writing it, so our own monitor does not
/// immediately broadcast it back to the machine it came from.
pub fn apply_remote_text(state: &Arc<Mutex<ClipState>>, text: &str) {
    {
        let mut st = state.lock().unwrap();
        st.text = Some(text.to_string());
        st.files.clear();
    }
    set_clipboard(text);
}

/// Same as [`apply_remote_text`] for a finished file transfer: write the paths onto the local
/// pasteboard and remember them so the monitor stays quiet.
pub fn apply_remote_files(state: &Arc<Mutex<ClipState>>, paths: &[PathBuf]) -> bool {
    let ok = clipfile::write_files(paths);
    let mut st = state.lock().unwrap();
    st.files = paths.to_vec();
    st.text = None;
    ok
}

/// Monitor the local clipboard (text **and** files) and push every genuine change to the peers.
pub fn start_monitor(state: Arc<Mutex<ClipState>>, net: Arc<Mutex<Net>>) {
    std::thread::spawn(move || {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("clipboard unavailable: {}", e);
                return;
            }
        };
        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));

            // ---- text ----
            if let Ok(t) = cb.get_text() {
                let changed = {
                    let st = state.lock().unwrap();
                    st.text.as_deref() != Some(t.as_str())
                };
                if changed {
                    {
                        let mut st = state.lock().unwrap();
                        st.text = Some(t.clone());
                        st.files.clear();
                    }
                    log::debug!("clipboard text changed ({} bytes)", t.len());
                    net.lock()
                        .unwrap()
                        .broadcast_clipboard(&t, None);
                }
            }

            // ---- files ----
            // A copy replaces whatever was on the pasteboard, so an empty list is the normal
            // case (text or nothing is copied) and is simply ignored.
            let files = clipfile::read_files();
            if !files.is_empty() {
                let changed = {
                    let st = state.lock().unwrap();
                    st.files != files
                };
                if changed {
                    {
                        let mut st = state.lock().unwrap();
                        st.files = files.clone();
                        st.text = None;
                    }
                    log::info!("clipboard files changed ({} item(s))", files.len());
                    crate::transfer::send_paths(net.clone(), files);
                }
            }
        }
    });
}

/// Re-broadcast a text clipboard update received from one peer to all the others (hub role).
pub fn relay(state: &Arc<Mutex<ClipState>>, net: &Arc<Mutex<Net>>, text: &str, from: &str) {
    apply_remote_text(state, text);
    net.lock().unwrap().broadcast_clipboard(text, Some(from));
}

/// Convenience for tests / future callers: is `msg` a clipboard frame?
pub fn is_clipboard(msg: &Message) -> bool {
    matches!(msg, Message::Clipboard { .. })
}
