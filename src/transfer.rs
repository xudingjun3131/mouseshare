//! Cross-machine **file copy**: manifest + chunked payload + reassembly.
//!
//! Text rides along in a single `Clipboard` frame; files cannot. Instead of base64-ing a whole
//! archive into one frame (which would mean buffering the entire copy in RAM on both ends and
//! stalling the input stream behind it), a copy is streamed as:
//!
//! ```text
//! ClipboardFiles { token, entries }   -- manifest: relative path + size per file
//! FileChunk      { token, seq, data } -- one `FILE_CHUNK` (64 KiB) of base64, one frame at a time
//! FileEnd        { token }            -- reassembly is complete
//! ```
//!
//! `token` makes concurrent copies independent, and `seq` lets the receiver drop a chunk that
//! arrived out of order rather than silently corrupting a file.
//!
//! The token has to be unique across the whole LAN, not merely within one process. The hub funnels
//! every peer's transfers through a *single* [`Receiver`] keyed by token, and a relayed copy keeps
//! its sender's token — so if each machine numbered its copies from 1, the second machine's first
//! copy would land on the first machine's token, take over its inbox directory and replace the
//! in-flight `Transfer` state. [`set_machine_name`] mixes a hash of the machine name into the high
//! half of every token to make that impossible.

use crate::network::Net;
use crate::protocol::{FileEntry, Message, FILE_CHUNK, MAX_FILE_BYTES};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// High 32 bits of every token: a hash of this machine's name. Zero until [`set_machine_name`] is
/// called, which `main` does at startup.
static MACHINE_TAG: AtomicU32 = AtomicU32::new(0);

/// Tell the transfer layer which machine it is, so its transfer tokens cannot collide with another
/// machine's. Must be called before the first copy is sent.
pub fn set_machine_name(name: &str) {
    MACHINE_TAG.store(fnv1a32(name), Ordering::Relaxed);
}

/// FNV-1a. Any stable hash will do — it only needs to spread machine names apart, not resist
/// attack, and it must not depend on `RandomState` (which differs per process and so would defeat
/// the point).
fn fnv1a32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn next_token() -> u64 {
    let seq = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed) & 0xFFFF_FFFF;
    ((MACHINE_TAG.load(Ordering::Relaxed) as u64) << 32) | seq
}

/// Where a received copy is reassembled. Kept outside the app's config directory so a huge
/// paste never ends up in iCloud/OneDrive-synced folders.
fn inbox_root() -> PathBuf {
    std::env::temp_dir().join("mouseshare").join("clipboard")
}

/// Upper bound on how many files one copy may expand to.
///
/// `collect` runs on the send thread and walks a copied directory tree with no progress output,
/// so an unbounded walk of something enormous (a home folder, `node_modules`, a `.app` bundle)
/// looks exactly like a hang: the user copies, nothing is ever sent, and nothing is logged —
/// the "mac can't copy files to the other machine" symptom. Cap the walk so it always finishes
/// and always says so.
const MAX_WALK_ENTRIES: usize = 20_000;
/// Recursion cap. Also breaks symlink cycles now that `collect_one` follows links.
const MAX_WALK_DEPTH: usize = 32;

/// Running budget for one `collect` walk.
struct Walk {
    truncated: bool,
}

/// Expand the copied selection into a flat list of `(relative path, absolute path)`.
///
/// Directories are walked recursively: the receiving side recreates the tree from the relative
/// paths and puts only the *top-level* entries on its pasteboard, which is what Finder and
/// Explorer expect from a paste.
pub fn collect(paths: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut walk = Walk { truncated: false };
    for p in paths {
        collect_one(p, p, 0, &mut out, &mut walk);
        if walk.truncated {
            break;
        }
    }
    if walk.truncated {
        log::warn!(
            "file copy walk truncated at {} entries; the copy is incomplete",
            MAX_WALK_ENTRIES
        );
        crate::diag::log(&format!(
            "FILE-SEND walk truncated at {} entries (copy is INCOMPLETE)",
            MAX_WALK_ENTRIES
        ));
    }
    out
}

fn collect_one(
    root: &Path,
    p: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
    walk: &mut Walk,
) {
    if walk.truncated {
        return;
    }
    if out.len() >= MAX_WALK_ENTRIES {
        walk.truncated = true;
        return;
    }
    // `metadata` (which follows symlinks) rather than `symlink_metadata`. The old code asked for
    // the link's *own* metadata, which reports neither `is_dir()` nor `is_file()` — so every
    // copied symlink fell through both arms and was silently dropped, and a selection made up
    // only of symlinks expanded to nothing and was reported as "nothing to send". Following the
    // link transfers the file the user actually sees in the Finder selection.
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) => {
            // A pasteboard path can name a file an app has already removed — chat and download
            // apps copy through a temp file, and clipboard managers can expose stale URLs. Name
            // the offending path so the log explains the failure instead of showing a bare
            // "nothing to send".
            log::warn!("skip {}: {}", p.display(), e);
            crate::diag::log(&format!(
                "FILE-SEND skip (unreadable) {}: {}",
                p.display(),
                e
            ));
            return;
        }
    };
    if meta.is_dir() {
        if depth >= MAX_WALK_DEPTH {
            crate::diag::log(&format!(
                "FILE-SEND skip (dir too deep, depth {}) {}",
                depth,
                p.display()
            ));
            return;
        }
        let Ok(entries) = std::fs::read_dir(p) else {
            // Unreadable directory (permissions, or a bundle we are not allowed into). Report it
            // rather than returning as if the directory were empty.
            crate::diag::log(&format!("FILE-SEND skip (dir unreadable) {}", p.display()));
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for child in entries {
            collect_one(root, &child, depth + 1, out, walk);
        }
    } else if meta.is_file() {
        let rel = p
            .strip_prefix(root.parent().unwrap_or(Path::new("")))
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, p.to_path_buf()));
    } else {
        // FIFO / socket / device node: not a regular file, so it cannot be streamed. Logged
        // because silently dropping part of a selection is worse than saying why.
        crate::diag::log(&format!("FILE-SEND skip (special file) {}", p.display()));
    }
}

/// Stream `paths` to the other machine(s). Spawns its own thread — reading and encoding a large
/// copy would otherwise block the clipboard monitor for seconds.
pub fn send_paths(net: Arc<Mutex<Net>>, paths: Vec<PathBuf>) {
    std::thread::spawn(move || {
        let all = collect(&paths);
        if all.is_empty() {
            // The selection collapsed to nothing: the pasteboard named paths that are not on
            // disk (a temp file an app already deleted), or only special files. Print the paths
            // so the log identifies the culprit, and tell the user, because a copy that
            // silently does nothing is indistinguishable from a broken feature.
            let shown: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            crate::diag::log(&format!(
                "FILE-SEND aborted: nothing to send (selection=[{}])",
                shown.join(", ")
            ));
            crate::app::notify(crate::i18n::tr_file_nothing_to_send());
            return;
        }
        // Open every file *before* building the manifest. The receiver sizes each file from the
        // manifest and walks the chunk stream in order, so a file we later fail to open would
        // leave it expecting bytes that never arrive — every subsequent file in the copy would
        // then be written with the wrong contents. Rejecting unreadable files up front keeps the
        // manifest and the payload exactly in step.
        let mut opened: Vec<(String, File, u64)> = Vec::new();
        for (rel, abs) in all {
            match File::open(&abs).and_then(|f| {
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                Ok((f, size))
            }) {
                Ok((f, size)) => opened.push((rel, f, size)),
                Err(e) => {
                    log::warn!("skip {}: {}", abs.display(), e);
                    crate::diag::log(&format!("FILE-SEND skip {}: {}", abs.display(), e));
                }
            }
        }
        if opened.is_empty() {
            crate::diag::log("FILE-SEND aborted: every file was unreadable");
            crate::app::notify(crate::i18n::tr_file_nothing_to_send());
            return;
        }
        let total: u64 = opened.iter().map(|(_, _, s)| *s).sum();
        if total > MAX_FILE_BYTES {
            log::warn!(
                "file copy too large ({} bytes, limit {}); skipped",
                total,
                MAX_FILE_BYTES
            );
            // Also to the *diagnostic* log, not just stderr: `log::warn!` goes to stderr, which
            // is discarded when the app is launched from Finder. Without this line an
            // over-limit copy left no trace at all in `mouseshare.log`, so a "copy does
            // nothing" report had to be diagnosed blind.
            crate::diag::log(&format!(
                "FILE-SEND aborted: {} bytes exceeds the {} byte limit (files={})",
                total,
                MAX_FILE_BYTES,
                opened.len()
            ));
            crate::app::notify(crate::i18n::tr_file_too_big());
            return;
        }
        let token = next_token();
        let entries: Vec<FileEntry> = opened
            .iter()
            .map(|(rel, _, size)| FileEntry {
                path: rel.clone(),
                size: *size,
            })
            .collect();
        let n = entries.len();
        log::info!("sending {} file(s), {} bytes (token {})", n, total, token);
        // One lock per frame, never held across a file read.
        //
        // The macOS event-tap callback locks the same `net` mutex to forward mouse motion, and a
        // callback that blocks for long gets the tap disabled by the OS — which is exactly the
        // "laggy, and both cursors move" symptom. Holding the lock for the whole copy (easy to
        // write, and what this used to do) blocks input for seconds on a large selection, so each
        // frame takes the lock only for the duration of its own send.
        let send = |msg: Message| {
            net.lock().unwrap().broadcast_all(msg);
        };
        send(Message::ClipboardFiles { token, entries });
        let mut seq = 0u64;
        let mut buf = vec![0u8; FILE_CHUNK];
        for (_, f, _) in opened.iter_mut() {
            loop {
                match f.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let data = base64_encode(&buf[..read]);
                        send(Message::FileChunk { token, seq, data });
                        seq += 1;
                        // Yield between chunks. Without this the copy thread fills the writer queue
                        // as fast as it can read the disk, and every mouse delta has to queue behind
                        // the payload — the cursor visibly stutters for the whole transfer. A
                        // one-millisecond pause per chunk costs a 27 MB copy about a third of a
                        // second and keeps the pointer smooth.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
        send(Message::FileEnd { token });
        log::info!("file copy sent (token {}, {} chunks)", token, n);
        crate::diag::log(&format!(
            "FILE-SEND token={} files={} bytes={}",
            token, n, total
        ));
        // Visible confirmation on the sending machine. Without this the user cannot tell whether
        // the copy was even detected (vs. silently swallowed by the monitor), and a failure
        // further down the wire reads as "file copy just doesn't work".
        crate::app::notify(crate::i18n::tr_file_sent(n));
    });
}

// ------------------------------------------------------------ receiver ---------------------

/// Turn a peer-supplied manifest path into a path that is safe to write **inside** the transfer
/// root.
///
/// A manifest arrives over the network, so it is untrusted input: `../../.ssh/authorized_keys`,
/// a bare `/etc/passwd`, and Windows drive/UNC forms all have to be neutralised before anything
/// touches the filesystem — otherwise a malicious (or merely buggy) peer picks where our process
/// writes. Every entry is sanitised, never dropped, so the number of files — and therefore the
/// byte accounting the sender relies on — stays exactly in step with the manifest. A path that
/// sanitises away to nothing becomes `unnamed`.
fn safe_rel(rel: &str) -> PathBuf {
    let normalised = rel.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in normalised.split('/') {
        // Empty (a leading `/` or a doubled separator), `.` and `..` are all stripped, so the
        // result can only ever be a descendant of the transfer root.
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        // `C:foo` — a drive designator addresses another volume, not a file in the root.
        let b = part.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            continue;
        }
        out.push(scrub_component(part));
    }
    if out.as_os_str().is_empty() {
        out.push("unnamed");
    }
    out
}

/// Make one path component creatable on the current platform.
fn scrub_component(part: &str) -> String {
    if !cfg!(windows) {
        return part.to_string();
    }
    // Windows rejects these characters outright, so a Mac file named `a:b.txt` would otherwise
    // fail `File::create` and abort the transfer at that file.
    let mut s: String = part
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // The Win32 layer silently strips trailing dots and spaces, which would make the created
    // name differ from the manifest and confuse Explorer's paste.
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    // Reserved device names stay unusable even with an extension (`CON.txt` is still CON).
    if is_windows_reserved(&s) {
        s.insert(0, '_');
    }
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// `CON`, `PRN`, `AUX`, `NUL`, and `COM1`..`COM9` / `LPT1`..`LPT9`. Matched case-insensitively
/// and ignoring any extension, because Windows does.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    if stem.len() == 4 {
        let (prefix, digit) = stem.split_at(3);
        if (prefix == "COM" || prefix == "LPT")
            && matches!(digit, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        {
            return true;
        }
    }
    false
}

struct Transfer {
    root: PathBuf,
    /// Sanitised relative destination plus expected size, in manifest order.
    files: VecDeque<(PathBuf, u64)>,
    /// The file currently being written — its sanitised path and size — plus the writer.
    current: Option<(PathBuf, u64, BufWriter<File>)>,
    written: u64,
    expect_seq: u64,
    top: Vec<PathBuf>,
}

/// Reassembles incoming file transfers. One instance per machine, driven from the incoming
/// message handler.
#[derive(Default)]
pub struct Receiver {
    active: std::collections::HashMap<u64, Transfer>,
}

impl Receiver {
    /// Feed one message. Returns the top-level paths of a copy that just finished, ready to be
    /// placed on the local pasteboard.
    pub fn handle(&mut self, msg: Message) -> Option<Vec<PathBuf>> {
        match msg {
            Message::ClipboardFiles { token, entries } => {
                let n = entries.len();
                let bytes: u64 = entries.iter().map(|e| e.size).sum();
                let names: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
                crate::diag::log(&format!(
                    "FILE-RECV-BEGIN token={} files={} bytes={} entries=[{}]",
                    token,
                    n,
                    bytes,
                    names.join(", ")
                ));
                self.begin(token, entries);
                None
            }
            Message::FileChunk { token, seq, data } => {
                if seq == 0 {
                    crate::diag::log(&format!("FILE-RECV-FIRST-CHUNK token={}", token));
                }
                self.chunk(token, seq, &data);
                None
            }
            Message::FileEnd { token } => {
                let out = self.finish(token);
                crate::diag::log(&format!(
                    "FILE-RECV-END token={} result={}",
                    token,
                    match &out {
                        Some(p) => format!("ok n={}", p.len()),
                        None => "none (transfer dropped or produced no files)".to_string(),
                    }
                ));
                out
            }
            _ => None,
        }
    }

    fn begin(&mut self, token: u64, entries: Vec<FileEntry>) {
        let root = inbox_root().join(token.to_string());
        // Sweep abandoned transfer directories — but only ones old enough that they cannot be a
        // copy still in flight. Deleting *every* other directory (the previous behaviour) would
        // pull the files out from under a second transfer that started a moment ago.
        if let Ok(dir) = std::fs::read_dir(inbox_root()) {
            let stale = Duration::from_secs(30 * 60);
            for e in dir.flatten() {
                if e.path() == root {
                    continue;
                }
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|d| d > stale).unwrap_or(false))
                    .unwrap_or(false);
                if old {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        if std::fs::create_dir_all(&root).is_err() {
            log::warn!("could not create transfer dir {}", root.display());
            return;
        }
        // Sanitise the whole manifest up front and keep the entry count unchanged, so the chunk
        // stream that follows stays aligned with it (see the note in `send_paths` about why the
        // manifest and payload must never drift apart).
        let files: VecDeque<(PathBuf, u64)> = entries
            .iter()
            .map(|e| (safe_rel(&e.path), e.size))
            .collect();
        // Re-check the size cap here as well. The sender caps the copy, but we do not trust a
        // remote peer's arithmetic with our disk.
        let bytes: u64 = entries.iter().map(|e| e.size).sum();
        if bytes > MAX_FILE_BYTES {
            log::warn!("refusing {} byte transfer (token {})", bytes, token);
            crate::diag::log(&format!(
                "FILE-RECV-REFUSED token={} bytes={} over limit",
                token, bytes
            ));
            return;
        }
        let mut top: Vec<PathBuf> = Vec::new();
        for (rel, _) in &files {
            if let Some(first) = rel.components().next() {
                let p = root.join(first.as_os_str());
                if !top.contains(&p) {
                    top.push(p);
                }
            }
        }
        log::info!(
            "receiving {} file(s) into {} (token {})",
            files.len(),
            root.display(),
            token
        );
        self.active.insert(
            token,
            Transfer {
                root,
                files,
                current: None,
                written: 0,
                expect_seq: 0,
                top,
            },
        );
    }

    fn chunk(&mut self, token: u64, seq: u64, data: &str) {
        let Some(t) = self.active.get_mut(&token) else {
            return;
        };
        // Frames are ordered on a single TCP connection, so a gap means we lost one; drop the
        // rest of the transfer rather than write a corrupt file.
        if seq != t.expect_seq {
            log::warn!(
                "file chunk out of order (got {}, want {})",
                seq,
                t.expect_seq
            );
            self.active.remove(&token);
            return;
        }
        t.expect_seq += 1;
        let bytes = base64_decode(data);
        let mut slice: &[u8] = &bytes;
        while !slice.is_empty() {
            // Open the next manifest entry when we are between files.
            if t.current.is_none() {
                let Some((rel, size)) = t.files.pop_front() else {
                    return; // more data than the manifest promised
                };
                let dest = t.root.join(&rel);
                // Defence in depth: `safe_rel` already guarantees a relative descendant of the
                // root, but a bug in it must not become an arbitrary file write.
                if !dest.starts_with(&t.root) {
                    crate::diag::log(&format!(
                        "FILE-RECV unsafe destination rejected: {}",
                        dest.display()
                    ));
                    self.active.remove(&token);
                    return;
                }
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match File::create(&dest) {
                    Ok(f) => t.current = Some((rel, size, BufWriter::new(f))),
                    Err(e) => {
                        log::warn!("cannot create {}: {}", dest.display(), e);
                        crate::diag::log(&format!(
                            "FILE-RECV cannot create {}: {}",
                            dest.display(),
                            e
                        ));
                        return;
                    }
                }
                t.written = 0;
            }
            let need = t
                .current
                .as_ref()
                .map(|(_, size, _)| size.saturating_sub(t.written))
                .unwrap_or(0);
            let take = slice.len().min(need as usize);
            let (head, tail) = slice.split_at(take);
            let done = {
                let (_, size, w) = t.current.as_mut().unwrap();
                if w.write_all(head).is_err() {
                    self.active.remove(&token);
                    return;
                }
                t.written += head.len() as u64;
                t.written >= *size
            };
            if done {
                if let Some((_, _, mut w)) = t.current.take() {
                    let _ = w.flush();
                }
                t.written = 0;
            }
            slice = tail;
        }
    }

    fn finish(&mut self, token: u64) -> Option<Vec<PathBuf>> {
        let mut t = self.active.remove(&token)?;
        if let Some((_, _, mut w)) = t.current.take() {
            let _ = w.flush();
        }
        // Materialise any remaining zero-byte entries. A file with no bytes never enters the
        // chunk loop, so without this a copied empty file would simply not appear on the far
        // side — and a selection made up only of empty files would vanish entirely.
        while let Some((rel, size)) = t.files.pop_front() {
            if size != 0 {
                break;
            }
            let dest = t.root.join(&rel);
            if !dest.starts_with(&t.root) {
                continue;
            }
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = File::create(&dest) {
                crate::diag::log(&format!(
                    "FILE-RECV cannot create {}: {}",
                    dest.display(),
                    e
                ));
            }
        }
        // Only hand back paths that really landed on disk. A truncated or dropped chunk leaves a
        // zero-length or missing file, and putting a non-existent path on the pasteboard makes
        // Cmd/Ctrl+V fail silently — which reads to the user as "file copy doesn't work".
        let top: Vec<PathBuf> = std::mem::take(&mut t.top)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if top.is_empty() {
            log::warn!("file transfer {} produced no usable files", token);
        }
        crate::diag::log(&format!("FILE-RECV token={} items={}", token, top.len()));
        Some(top)
    }
}

// -------------------------------------------------------------- base64 ---------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len() / 3 * 4 + 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in input.bytes() {
        if c == b'=' {
            break;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for len in [0usize, 1, 2, 3, 4, 5, 255, 1000, 4096] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let enc = base64_encode(&data);
            assert_eq!(base64_decode(&enc), data, "len {}", len);
        }
    }

    #[test]
    fn collect_flattens_directories() {
        let root = std::env::temp_dir().join("mouseshare-test-collect");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), b"bb").unwrap();

        let files = collect(&[root.clone()]);
        let rels: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
        assert!(rels.iter().any(|r| r.ends_with("a.txt")), "{:?}", rels);
        assert!(rels.iter().any(|r| r.ends_with("b.txt")), "{:?}", rels);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_keeps_symlinks_and_empty_files() {
        let root = std::env::temp_dir().join("mouseshare-test-collect-symlink");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("target.txt"), b"hi").unwrap();
        std::fs::write(root.join("empty.txt"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("target.txt"), root.join("link.txt")).unwrap();

        let files = collect(&[root.clone()]);
        let rels: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
        // A symlink used to be dropped silently (its *own* metadata is neither dir nor file), so
        // a selection of symlinks expanded to nothing and was reported as "nothing to send".
        #[cfg(unix)]
        assert!(rels.iter().any(|r| r.ends_with("link.txt")), "{:?}", rels);
        assert!(rels.iter().any(|r| r.ends_with("empty.txt")), "{:?}", rels);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_rel_neutralises_traversal_and_absolute_paths() {
        assert_eq!(safe_rel("../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(safe_rel("/etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(safe_rel("a/../b"), PathBuf::from("a/b"));
        assert_eq!(safe_rel(".."), PathBuf::from("unnamed"));
        assert_eq!(safe_rel(""), PathBuf::from("unnamed"));
        assert_eq!(
            safe_rel("C:\\Windows\\win.ini"),
            PathBuf::from("Windows/win.ini")
        );
        // A legitimate nested tree keeps its shape.
        assert_eq!(safe_rel("dir/sub/f.txt"), PathBuf::from("dir/sub/f.txt"));
    }

    #[test]
    fn windows_reserved_names_are_detected() {
        assert!(is_windows_reserved("CON"));
        assert!(is_windows_reserved("con.txt"));
        assert!(is_windows_reserved("COM1"));
        assert!(is_windows_reserved("LPT9.log"));
        assert!(!is_windows_reserved("COM0"));
        assert!(!is_windows_reserved("console.txt"));
        assert!(!is_windows_reserved("null.txt"));
    }

    #[test]
    fn receiver_confines_manifest_paths_to_the_inbox() {
        let mut rx = Receiver::default();
        let token = 9_000_001;
        let payload = b"pwned";
        rx.handle(Message::ClipboardFiles {
            token,
            entries: vec![
                FileEntry {
                    path: "../../../evil.txt".into(),
                    size: payload.len() as u64,
                },
                FileEntry {
                    path: "/etc/evil.txt".into(),
                    size: payload.len() as u64,
                },
            ],
        });
        rx.handle(Message::FileChunk {
            token,
            seq: 0,
            data: base64_encode(&[payload.as_slice(), payload.as_slice()].concat()),
        });
        let out = rx
            .handle(Message::FileEnd { token })
            .expect("transfer completes");

        let inbox = std::fs::canonicalize(inbox_root()).unwrap_or_else(|_| inbox_root());
        assert_eq!(out.len(), 2, "both entries should land: {:?}", out);
        for p in &out {
            let c = std::fs::canonicalize(p).expect("received path exists");
            assert!(
                c.starts_with(&inbox),
                "received path escaped the inbox: {}",
                c.display()
            );
        }
        let _ = std::fs::remove_dir_all(inbox_root().join(token.to_string()));
    }

    #[test]
    fn receiver_materialises_zero_byte_files() {
        let mut rx = Receiver::default();
        let token = 9_000_002;
        rx.handle(Message::ClipboardFiles {
            token,
            entries: vec![FileEntry {
                path: "empty.txt".into(),
                size: 0,
            }],
        });
        // No chunk follows: a zero-byte file never enters the write loop.
        let out = rx
            .handle(Message::FileEnd { token })
            .expect("transfer completes");
        assert_eq!(
            out.len(),
            1,
            "empty file should still be created: {:?}",
            out
        );
        assert_eq!(std::fs::metadata(&out[0]).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(inbox_root().join(token.to_string()));
    }

    /// A token must be unique across the LAN, not just within one process. The hub funnels every
    /// peer's transfers through one `Receiver` keyed by token and relayed copies keep the sender's
    /// token, so if each machine counted from 1 then two machines starting a copy at the same time
    /// would both pick token 1: the second `begin` would take over the first's inbox directory and
    /// replace its in-flight state, and chunks from the two files would interleave.
    #[test]
    fn tokens_are_namespaced_per_machine() {
        set_machine_name("machine-a");
        let a: Vec<u64> = (0..4).map(|_| next_token()).collect();
        set_machine_name("machine-b");
        let b: Vec<u64> = (0..4).map(|_| next_token()).collect();

        for x in &a {
            for y in &b {
                assert_ne!(x, y, "token {x} collides across machines");
            }
        }
        // Still a monotonic sequence within one machine, so ordering is preserved.
        assert!(a[0] < a[1] && a[1] < a[2] && a[2] < a[3]);
        assert!(b[0] < b[1]);
        // The machine half is what separates them, and it is a real signature of the name.
        assert_ne!(a[0] >> 32, b[0] >> 32);
        assert_eq!(a[0] >> 32, fnv1a32("machine-a") as u64);
        // A different name gives a different tag, so the namespace is not accidental.
        set_machine_name("machine-c");
        assert_ne!(next_token() >> 32, a[0] >> 32);

        // Leave a defined tag behind: `main` always sets one before any copy is sent.
        set_machine_name("machine-a");
    }
}
