//! Single-instance guard — refuse to run two copies of MouseShare at once.
//!
//! A second copy is never useful: the primary would fail to bind the listen port (and, before
//! this guard existed, that failure surfaced as a confusing "cannot listen" error rather than
//! "already running"), and two capture taps fighting over the same cursor produce erratic
//! motion. So we take an OS-level lock at startup and bail out if it is already held.

/// Keeps the instance lock alive for the process lifetime. Dropping it releases the lock.
pub struct Guard {
    #[cfg(target_os = "windows")]
    handle: *mut std::ffi::c_void,
    #[cfg(not(target_os = "windows"))]
    _file: std::fs::File,
}

// The handle is only ever closed in `Drop`; it is never dereferenced, so it is safe to move
// between threads (in practice the guard lives on the main thread for the whole run).
unsafe impl Send for Guard {}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        unsafe {
            if !self.handle.is_null() {
                CloseHandle(self.handle);
            }
        }
    }
}

/// Take the single-instance lock. Returns `None` when another instance already holds it.
pub fn acquire() -> Option<Guard> {
    #[cfg(target_os = "windows")]
    {
        // A named mutex in the Global\ namespace is visible across sessions and is released
        // automatically if the process dies.
        let name: Vec<u16> = "Global\\MouseShare.SingleInstance"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if h.is_null() {
                // Cannot create the mutex — fail open rather than refusing to start.
                return Some(Guard { handle: std::ptr::null_mut() });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return None;
            }
            Some(Guard { handle: h })
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // An flock on a well-known lock file. flock is released by the kernel when the
        // process exits, including on a crash, so a stale lock cannot wedge the app.
        let path = std::env::temp_dir().join("mouseshare.single.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc != 0 {
            return None;
        }
        Some(Guard { _file: file })
    }
}

#[cfg(target_os = "windows")]
const ERROR_ALREADY_EXISTS: u32 = 183;

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(
        attrs: *const std::ffi::c_void,
        initial_owner: i32,
        name: *const u16,
    ) -> *mut std::ffi::c_void;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
}

#[cfg(not(target_os = "windows"))]
use std::os::unix::io::AsRawFd;

#[cfg(not(target_os = "windows"))]
const LOCK_EX: i32 = 2;
#[cfg(not(target_os = "windows"))]
const LOCK_NB: i32 = 4;

#[cfg(not(target_os = "windows"))]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// Tell the user a copy is already running.
///
/// On Windows the binary is built for the GUI subsystem, so stdout/stderr go nowhere — a
/// plain `eprintln!` would be invisible and the second launch would look like it silently
/// did nothing (the exact complaint this guard fixes). Show a real dialog there.
pub fn notify_already_running() {
    #[cfg(target_os = "windows")]
    {
        let title: Vec<u16> = "MouseShare".encode_utf16().chain(std::iter::once(0)).collect();
        let text: Vec<u16> = "MouseShare 已在运行中。\n\n请检查任务栏通知区域（托盘）里的 MouseShare 图标。"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("MouseShare is already running (lock held). Exiting this instance.");
    }
}

#[cfg(target_os = "windows")]
const MB_OK: u32 = 0x0000_0000;
#[cfg(target_os = "windows")]
const MB_ICONINFORMATION: u32 = 0x0000_0040;
#[cfg(target_os = "windows")]
const MB_SETFOREGROUND: u32 = 0x0001_0000;

#[cfg(target_os = "windows")]
#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(hwnd: *mut std::ffi::c_void, text: *const u16, caption: *const u16, ty: u32)
        -> i32;
}
