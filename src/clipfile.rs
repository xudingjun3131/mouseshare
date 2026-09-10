//! Read and write **file** clipboard contents (Finder copy / Explorer copy).
//!
//! `arboard` — the crate used for text — deliberately covers only text and images, so
//! cross-machine file copy needs the native pasteboard:
//!
//! * **macOS** — `NSPasteboard` items carrying `public.file-url` (a `file://` URL per file).
//! * **Windows** — the `CF_HDROP` format (a `DROPFILES` header plus a double-null-terminated
//!   list of wide paths).
//! * **Linux** — not wired up (X11 selections vary by desktop); file copy is a no-op there.
//!
//! Everything is deliberately read-only-by-value: we hand back plain `PathBuf`s and let
//! [`crate::transfer`] worry about moving bytes between machines.

use std::path::PathBuf;

/// The file paths currently on the system pasteboard (empty when the clipboard holds text,
/// an image, or nothing).
pub fn read_files() -> Vec<PathBuf> {
    imp::read_files()
}

/// Put `paths` on the system pasteboard as files, so Cmd/Ctrl+V in Finder/Explorer pastes them.
/// Returns `false` when the platform cannot do it or the clipboard could not be opened.
pub fn write_files(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    imp::write_files(paths)
}

/// Whether this platform implements file clipboard contents at all.
///
/// Callers use this to avoid retrying: on Linux `write_files` is a no-op that always fails, and
/// retrying a write that can never succeed would stall the network reader thread — and with it
/// the forwarded input sharing that thread — for a feature that does not exist there.
pub fn supports_files() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// A counter that the OS increments on **every** change to the clipboard, however small.
///
/// Needed because "did the clipboard change?" cannot be answered by comparing *contents*: copy
/// the same file twice and the paths are identical, so a content comparison sees no change and
/// silently skips the second copy — the user presses Cmd+C again, nothing happens, and the
/// feature reads as broken. The generation number changes even when the contents do not.
///
/// `None` on platforms that have no such counter (callers then fall back to comparing content).
pub fn generation() -> Option<i64> {
    imp::generation()
}

// ---------------------------------------------------------------- macOS -------------------

#[cfg(target_os = "macos")]
mod imp {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel};
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::path::PathBuf;

    /// `public.file-url` — the modern (10.10+) pasteboard type for copied files.
    const FILE_URL_TYPE: &str = "public.file-url";
    /// `NSFilenamesPboardType` — the pre-10.10 type. Finder still populates it, and some apps
    /// (and some cross-platform toolkits) write *only* this one, so reading it as well is the
    /// difference between "file copy works" and "file copy silently does nothing".
    const FILENAMES_TYPE: &str = "NSFilenamesPboardType";

    unsafe fn nsstring(s: &str) -> *mut Object {
        let c = match CString::new(s) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    unsafe fn to_string(s: *mut Object) -> Option<String> {
        if s.is_null() {
            return None;
        }
        let p: *const c_char = msg_send![s, UTF8String];
        if p.is_null() {
            return None;
        }
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }

    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    /// `file:///Users/a/b%20c.txt` -> `/Users/a/b c.txt`
    fn url_to_path(url: &str) -> Option<PathBuf> {
        let rest = url.strip_prefix("file://")?;
        // `file:///x` (empty host) or `file://localhost/x`.
        let rest = match rest.strip_prefix("localhost") {
            Some(r) => r,
            None => rest,
        };
        let mut out: Vec<u8> = Vec::with_capacity(rest.len());
        let b = rest.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
    }

    fn path_to_url(p: &std::path::Path) -> String {
        let s = p.to_string_lossy();
        let mut out = String::with_capacity(s.len() + 8);
        out.push_str("file://");
        for b in s.bytes() {
            // Leave the path separators and unreserved characters alone; percent-encode
            // everything else so spaces and non-ASCII names survive.
            let unreserved = matches!(
                b,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/'
            );
            if unreserved {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        out
    }

    /// Every path currently on the pasteboard, tried through each representation macOS may use.
    ///
    /// There is no single reliable way to ask "is a file on the clipboard?": Finder populates
    /// `pasteboardItems` with `public.file-url` *and* the legacy `NSFilenamesPboardType`, but
    /// other apps (and some cross-platform toolkits) write only one of them. Reading just one
    /// representation — the obvious implementation — makes file copy work from Finder and
    /// silently do nothing everywhere else. So try each in turn and return the first hit.
    pub fn read_files() -> Vec<PathBuf> {
        objc::rc::autoreleasepool(|| unsafe {
            let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
            if pb.is_null() {
                return Vec::new();
            }

            // 1. Per-item `public.file-url` (the modern, multi-file representation).
            let mut out = read_item_urls(pb);
            if !out.is_empty() {
                return out;
            }

            // 2. A single `public.file-url` written directly on the pasteboard (old-style API).
            let ty = nsstring(FILE_URL_TYPE);
            if !ty.is_null() {
                let s: *mut Object = msg_send![pb, stringForType: ty];
                if let Some(p) = to_string(s).and_then(|u| url_to_path(&u)) {
                    out.push(p);
                    return out;
                }
            }

            // 3. The legacy `NSFilenamesPboardType`: an NSArray of plain path strings.
            out = read_filenames(pb);
            out
        })
    }

    /// Walk `pasteboardItems`, pulling a `public.file-url` out of each.
    unsafe fn read_item_urls(pb: *mut Object) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let items: *mut Object = msg_send![pb, pasteboardItems];
        if items.is_null() {
            return out;
        }
        let count: usize = msg_send![items, count];
        let ty = nsstring(FILE_URL_TYPE);
        if ty.is_null() {
            return out;
        }
        for i in 0..count {
            let item: *mut Object = msg_send![items, objectAtIndex: i];
            let s: *mut Object = msg_send![item, stringForType: ty];
            if let Some(p) = to_string(s).and_then(|u| url_to_path(&u)) {
                out.push(p);
            }
        }
        out
    }

    /// Read the legacy `NSFilenamesPboardType` (an `NSArray<NSString*>` of absolute paths).
    unsafe fn read_filenames(pb: *mut Object) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let ty = nsstring(FILENAMES_TYPE);
        if ty.is_null() {
            return out;
        }
        let arr: *mut Object = msg_send![pb, propertyListForType: ty];
        if arr.is_null() {
            return out;
        }
        let count: usize = msg_send![arr, count];
        for i in 0..count {
            let s: *mut Object = msg_send![arr, objectAtIndex: i];
            if let Some(p) = to_string(s) {
                out.push(PathBuf::from(p));
            }
        }
        out
    }

    pub fn write_files(paths: &[PathBuf]) -> bool {
        objc::rc::autoreleasepool(|| unsafe {
            let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
            if pb.is_null() {
                return false;
            }
            let arr: *mut Object = msg_send![class!(NSMutableArray), array];
            let ty = nsstring(FILE_URL_TYPE);
            if arr.is_null() || ty.is_null() {
                return false;
            }
            for p in paths {
                let s = nsstring(&path_to_url(p));
                if s.is_null() {
                    continue;
                }
                let item: *mut Object = msg_send![class!(NSPasteboardItem), alloc];
                let item: *mut Object = msg_send![item, init];
                let _: () = msg_send![item, setString: s forType: ty];
                let _: () = msg_send![arr, addObject: item];
                // The array retained it; drop our own reference.
                let _: () = msg_send![item, release];
            }
            let _: isize = msg_send![pb, clearContents];
            let ok: bool = msg_send![pb, writeObjects: arr];

            // Also publish the legacy `NSFilenamesPboardType` so apps that only look for the
            // pre-10.10 type can paste what we received. This must come *after* `writeObjects:`,
            // which clears the pasteboard as part of writing and would otherwise wipe it again.
            if ok {
                let legacy = nsstring(FILENAMES_TYPE);
                let names: *mut Object = msg_send![class!(NSMutableArray), array];
                if !legacy.is_null() && !names.is_null() {
                    for p in paths {
                        let s = nsstring(&p.to_string_lossy());
                        if !s.is_null() {
                            let _: () = msg_send![names, addObject: s];
                        }
                    }
                    let _: bool = msg_send![pb, setPropertyList: names forType: legacy];
                }
            }
            ok
        })
    }

    /// `NSPasteboard.changeCount` — bumped on every write, including a re-copy of identical
    /// contents. This is what lets the monitor notice a second Cmd+C of the same file.
    pub fn generation() -> Option<i64> {
        objc::rc::autoreleasepool(|| unsafe {
            let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
            if pb.is_null() {
                return None;
            }
            let n: i64 = msg_send![pb, changeCount];
            Some(n)
        })
    }
}

// --------------------------------------------------------------- Windows -------------------

#[cfg(target_os = "windows")]
mod imp {
    use std::os::raw::c_void;
    use std::path::PathBuf;
    use std::time::Duration;

    const CF_HDROP: u32 = 15;
    const GMEM_MOVEABLE: u32 = 0x0002;
    /// DROPFILES is 20 bytes on both 32- and 64-bit Windows (four DWORDs + two LONGs).
    const DROPFILES_WORDS: usize = 5;
    const DROPFILES_BYTES: usize = DROPFILES_WORDS * 4;

    /// Windows refuses `OpenClipboard` while *any* other process holds the clipboard open, and
    /// plenty of background apps (input methods, Office, clipboard managers, screen readers)
    /// grab it for a few milliseconds at a time. A single attempt therefore fails often enough
    /// to matter: on the receiving side a lost write means the pasted file never appears, and on
    /// the sending side a lost read means we never notice the copy at all. Both look to the user
    /// like "file copy just doesn't work", so retry with a short back-off.
    fn open_clipboard_retrying() -> bool {
        for attempt in 0..10u64 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(15 * (attempt + 1)));
        }
        false
    }

    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        pt_x: i32,
        pt_y: i32,
        f_nc: i32,
        f_wide: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
    }
    #[link(name = "shell32")]
    extern "system" {
        fn DragQueryFileW(drop: *mut c_void, file: u32, buffer: *mut u16, buf_len: u32) -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetClipboardSequenceNumber() -> u32;
    }

    pub fn read_files() -> Vec<PathBuf> {
        unsafe {
            if IsClipboardFormatAvailable(CF_HDROP) == 0 {
                return Vec::new();
            }
            if !open_clipboard_retrying() {
                crate::diag::log("CLIP-FILES-READ-FAILED (OpenClipboard busy after retries)");
                return Vec::new();
            }
            let mut out = Vec::new();
            let handle = GetClipboardData(CF_HDROP);
            if !handle.is_null() {
                let count = DragQueryFileW(handle, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
                for i in 0..count {
                    let len = DragQueryFileW(handle, i, std::ptr::null_mut(), 0) as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len + 1];
                    let copied =
                        DragQueryFileW(handle, i, buf.as_mut_ptr(), (len + 1) as u32) as usize;
                    if copied > 0 {
                        out.push(PathBuf::from(String::from_utf16_lossy(&buf[..copied])));
                    }
                }
            }
            CloseClipboard();
            out
        }
    }

    pub fn write_files(paths: &[PathBuf]) -> bool {
        unsafe {
            if !open_clipboard_retrying() {
                crate::diag::log("FILE-APPLY-FAILED (OpenClipboard busy after retries)");
                return false;
            }
            let mut ok = false;
            if EmptyClipboard() != 0 {
                // Double-null-terminated list of wide paths, preceded by DROPFILES.
                let mut names: Vec<u16> = Vec::new();
                for p in paths {
                    names.extend(p.to_string_lossy().encode_utf16());
                    names.push(0);
                }
                names.push(0);
                let total = DROPFILES_BYTES + names.len() * 2;
                let mem = GlobalAlloc(GMEM_MOVEABLE, total);
                if !mem.is_null() {
                    let base = GlobalLock(mem) as *mut u8;
                    if !base.is_null() {
                        let hdr = base as *mut DropFiles;
                        (*hdr).p_files = DROPFILES_BYTES as u32;
                        (*hdr).pt_x = 0;
                        (*hdr).pt_y = 0;
                        (*hdr).f_nc = 0;
                        (*hdr).f_wide = 1; // paths are UTF-16
                        std::ptr::copy_nonoverlapping(
                            names.as_ptr(),
                            base.add(DROPFILES_BYTES) as *mut u16,
                            names.len(),
                        );
                        GlobalUnlock(mem);
                        // On success the clipboard owns `mem`; do NOT free it.
                        ok = !SetClipboardData(CF_HDROP, mem).is_null();
                    }
                }
            }
            CloseClipboard();
            ok
        }
    }

    /// `GetClipboardSequenceNumber` — increments on every clipboard change, so re-copying the
    /// same file (identical `CF_HDROP` contents) is still detected as a new copy.
    pub fn generation() -> Option<i64> {
        unsafe { Some(GetClipboardSequenceNumber() as i64) }
    }
}

// ----------------------------------------------------------------- Linux / other ----------

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use std::path::PathBuf;
    pub fn read_files() -> Vec<PathBuf> {
        Vec::new()
    }
    pub fn write_files(_paths: &[PathBuf]) -> bool {
        false
    }
    pub fn generation() -> Option<i64> {
        None
    }
}
