//! Cross-machine **file copy**: manifest + chunked payload + reassembly.
//!
//! Text rides along in a single `Clipboard` frame; files cannot. Instead of base64-ing a whole
//! archive into one frame (which would mean buffering the entire copy in RAM on both ends and
//! stalling the input stream behind it), a copy is streamed as:
//!
//! ```text
//! ClipboardFiles { token, entries }   -- manifest: relative path + size per file
//! FileChunk      { token, seq, data } -- 256 KiB of base64, one frame at a time
//! FileEnd        { token }            -- reassembly is complete
//! ```
//!
//! `token` makes concurrent copies from different machines independent, and `seq` lets the
//! receiver drop a chunk that arrived out of order rather than silently corrupting a file.

use crate::network::Net;
use crate::protocol::{FileEntry, Message, FILE_CHUNK, MAX_FILE_BYTES};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Where a received copy is reassembled. Kept outside the app's config directory so a huge
/// paste never ends up in iCloud/OneDrive-synced folders.
fn inbox_root() -> PathBuf {
    std::env::temp_dir().join("mouseshare").join("clipboard")
}

/// Expand the copied selection into a flat list of `(relative path, absolute path)`.
///
/// Directories are walked recursively: the receiving side recreates the tree from the relative
/// paths and puts only the *top-level* entries on its pasteboard, which is what Finder and
/// Explorer expect from a paste.
pub fn collect(paths: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for p in paths {
        collect_one(p, p, &mut out);
    }
    out
}

fn collect_one(root: &Path, p: &Path, out: &mut Vec<(String, PathBuf)>) {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(p) else { return };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for child in entries {
            collect_one(root, &child, out);
        }
    } else if meta.is_file() {
        let rel = p
            .strip_prefix(root.parent().unwrap_or(Path::new("")))
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, p.to_path_buf()));
    }
}

/// Stream `paths` to the other machine(s). Spawns its own thread — reading and encoding a large
/// copy would otherwise block the clipboard monitor for seconds.
pub fn send_paths(net: Arc<Mutex<Net>>, paths: Vec<PathBuf>) {
    std::thread::spawn(move || {
        let files = collect(&paths);
        if files.is_empty() {
            return;
        }
        let total: u64 = files
            .iter()
            .map(|(_, abs)| std::fs::metadata(abs).map(|m| m.len()).unwrap_or(0))
            .sum();
        if total > MAX_FILE_BYTES {
            log::warn!(
                "file copy too large ({} bytes, limit {}); skipped",
                total,
                MAX_FILE_BYTES
            );
            crate::app::notify(crate::i18n::tr_file_too_big());
            return;
        }
        let token = next_token();
        let entries: Vec<FileEntry> = files
            .iter()
            .map(|(rel, abs)| FileEntry {
                path: rel.clone(),
                size: std::fs::metadata(abs).map(|m| m.len()).unwrap_or(0),
            })
            .collect();
        let n = files.len();
        log::info!("sending {} file(s), {} bytes (token {})", n, total, token);
        {
            let net = net.lock().unwrap();
            net.broadcast_all(Message::ClipboardFiles { token, entries });
            let mut seq = 0u64;
            let mut buf = vec![0u8; FILE_CHUNK];
            for (_, abs) in &files {
                let mut f = match File::open(abs) {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!("skip {}: {}", abs.display(), e);
                        continue;
                    }
                };
                loop {
                    match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let data = base64_encode(&buf[..read]);
                            net.broadcast_all(Message::FileChunk { token, seq, data });
                            seq += 1;
                        }
                    }
                }
            }
            net.broadcast_all(Message::FileEnd { token });
        }
        crate::diag::log(&format!("FILE-SEND token={} files={} bytes={}", token, n, total));
    });
}

// ------------------------------------------------------------ receiver ---------------------

struct Transfer {
    root: PathBuf,
    files: VecDeque<FileEntry>,
    /// The file currently being written plus how many of its bytes have arrived.
    current: Option<(FileEntry, BufWriter<File>)>,
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
                self.begin(token, entries);
                None
            }
            Message::FileChunk { token, seq, data } => {
                self.chunk(token, seq, &data);
                None
            }
            Message::FileEnd { token } => self.finish(token),
            _ => None,
        }
    }

    fn begin(&mut self, token: u64, entries: Vec<FileEntry>) {
        let root = inbox_root().join(token.to_string());
        // Keep the inbox from growing without bound: drop any older transfer's directory.
        if let Ok(dir) = std::fs::read_dir(inbox_root()) {
            for e in dir.flatten() {
                if e.path() != root {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        if std::fs::create_dir_all(&root).is_err() {
            log::warn!("could not create transfer dir {}", root.display());
            return;
        }
        let mut top: Vec<PathBuf> = Vec::new();
        for e in &entries {
            let first = e
                .path
                .split('/')
                .next()
                .unwrap_or(&e.path)
                .to_string();
            let p = root.join(&first);
            if !top.contains(&p) {
                top.push(p);
            }
        }
        log::info!(
            "receiving {} file(s) into {} (token {})",
            entries.len(),
            root.display(),
            token
        );
        self.active.insert(
            token,
            Transfer {
                root,
                files: entries.into(),
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
            log::warn!("file chunk out of order (got {}, want {})", seq, t.expect_seq);
            self.active.remove(&token);
            return;
        }
        t.expect_seq += 1;
        let bytes = base64_decode(data);
        let mut slice: &[u8] = &bytes;
        while !slice.is_empty() {
            // Open the next manifest entry when we are between files.
            if t.current.is_none() {
                let Some(entry) = t.files.pop_front() else {
                    return; // more data than the manifest promised
                };
                let dest = t.root.join(&entry.path);
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match File::create(&dest) {
                    Ok(f) => t.current = Some((entry, BufWriter::new(f))),
                    Err(e) => {
                        log::warn!("cannot create {}: {}", dest.display(), e);
                        return;
                    }
                }
                t.written = 0;
            }
            let need = t
                .current
                .as_ref()
                .map(|(e, _)| e.size.saturating_sub(t.written))
                .unwrap_or(0);
            let take = slice.len().min(need as usize);
            let (head, tail) = slice.split_at(take);
            let done = {
                let (entry, w) = t.current.as_mut().unwrap();
                if w.write_all(head).is_err() {
                    self.active.remove(&token);
                    return;
                }
                t.written += head.len() as u64;
                t.written >= entry.size
            };
            if done {
                if let Some((_, mut w)) = t.current.take() {
                    let _ = w.flush();
                }
                t.written = 0;
            }
            slice = tail;
        }
    }

    fn finish(&mut self, token: u64) -> Option<Vec<PathBuf>> {
        let mut t = self.active.remove(&token)?;
        if let Some((_, mut w)) = t.current.take() {
            let _ = w.flush();
        }
        crate::diag::log(&format!("FILE-RECV token={} items={}", token, t.top.len()));
        Some(std::mem::take(&mut t.top))
    }
}

// -------------------------------------------------------------- base64 ---------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len() / 3 * 4 + 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
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
}
