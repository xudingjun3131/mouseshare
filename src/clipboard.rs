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
    /// The pasteboard generation counter at the moment we last wrote to it (locally or from a
    /// remote). The monitor compares against the live counter rather than against contents, so a
    /// second Cmd+C of the *same* file is still seen as a fresh copy.
    pub gen: Option<i64>,
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
    // Our own write bumped the generation counter; remember the new value so the monitor sees it
    // as "ours" and stays quiet.
    {
        let mut st = state.lock().unwrap();
        st.gen = clipfile::generation();
    }
}

/// Same as [`apply_remote_text`] for a finished file transfer: write the paths onto the local
/// pasteboard and remember them so the monitor stays quiet.
pub fn apply_remote_files(state: &Arc<Mutex<ClipState>>, paths: &[PathBuf]) -> bool {
    let ok = clipfile::write_files(paths);
    let mut st = state.lock().unwrap();
    st.files = paths.to_vec();
    st.text = None;
    st.gen = clipfile::generation();
    ok
}

/// Monitor the local clipboard (text **and** files) and push every genuine change to the peers.
///
/// ## Why text and files are read *together*
///
/// A single paste is one pasteboard state that happens to have several representations: copying
/// a file in Finder puts both a `public.file-url` **and** a plain-text form (usually the file
/// name) on the pasteboard. Comparing the two independently — the obvious implementation — makes
/// them fight: the text branch fires, clears `files`, and broadcasts the name; the file branch
/// then sees `files` as "changed", clears `text`, and sends the file; next tick the text branch
/// fires again … forever. The user sees the remote clipboard flip between a file and the string
/// "report.pdf" every 200 ms, pasting usually lands on the text form, and the machine is busy
/// re-encoding and re-sending the same copy — which also shows up as input lag.
///
/// So: read both, and let **files win**. When a paste carries files we record the text we saw
/// alongside it, so the text branch cannot mistake that same text for a fresh copy.
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

            // The pasteboard generation counter is the *primary* change signal: it advances on
            // every write even when the contents are unchanged, so a second copy of the same file
            // (identical paths) is still detected. Content comparison is kept as the fallback for
            // platforms with no counter (Linux).
            let gen = clipfile::generation();
            let gen_changed = match (gen, state.lock().unwrap().gen) {
                (Some(g), Some(last)) => g != last,
                _ => true, // no counter: fall through to content comparison
            };
            if !gen_changed {
                continue;
            }

            // One snapshot of the pasteboard — see the note above about why these two reads
            // must be considered together rather than as independent channels.
            let text: Option<String> = cb.get_text().ok();
            let files = clipfile::read_files();

            if !files.is_empty() {
                // ---- a file paste (files win) ----
                let changed = {
                    let st = state.lock().unwrap();
                    // On a counter-less platform `gen_changed` is always true, so gate on content.
                    gen_changed || st.files != files
                };
                if changed {
                    {
                        let mut st = state.lock().unwrap();
                        st.files = files.clone();
                        // Remember the text that rides along with the file paste (usually the
                        // file name) so it is not re-detected as a new copy on the next tick.
                        st.text = text;
                        st.gen = gen;
                    }
                    log::info!("clipboard files changed ({} item(s))", files.len());
                    crate::diag::log(&format!(
                        "CLIP-FILES-DETECTED n={} gen={:?}",
                        files.len(),
                        gen
                    ));
                    crate::transfer::send_paths(net.clone(), files);
                }
            } else if let Some(t) = text {
                // ---- a text paste ----
                let changed = {
                    let st = state.lock().unwrap();
                    gen_changed || st.text.as_deref() != Some(t.as_str())
                };
                if changed {
                    {
                        let mut st = state.lock().unwrap();
                        st.text = Some(t.clone());
                        st.files.clear();
                        st.gen = gen;
                    }
                    log::debug!("clipboard text changed ({} bytes)", t.len());
                    net.lock().unwrap().broadcast_clipboard(&t, None);
                }
            } else {
                // Neither files nor text: record the generation so a later same-content write is
                // still detected, and clear our stale snapshot.
                let mut st = state.lock().unwrap();
                st.gen = gen;
                st.files.clear();
                st.text = None;
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
