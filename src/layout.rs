//! Screen layout model: a set of rectangles placed in a virtual desktop coordinate space.
//! Each rectangle represents one physical machine's screen.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screen {
    /// Unique machine name (must match that machine's `Config.name`).
    pub name: String,
    /// Top-left corner in virtual-desktop coordinates.
    pub ox: i32,
    pub oy: i32,
    /// Screen size in pixels.
    pub w: u32,
    pub h: u32,
    /// `true` for a display that physically belongs to *this* machine (the primary's own
    /// monitors). Input landing on a local screen is never forwarded — the real cursor is
    /// already there. `false` marks a remote (secondary) screen, which receives injected input.
    /// This flag (rather than comparing `name` to `primary_name`) is what lets the primary have
    /// more than one local display: every one of its monitors is `is_local = true` while still
    /// carrying a unique `name`.
    #[serde(default = "default_is_local")]
    pub is_local: bool,
    /// UI scale factor of the source display (1.0 = no scaling). Coordinates stay in the OS
    /// logical space that rdev reports (points on macOS, physical pixels once the Windows
    /// process is DPI-aware); this field only annotates the real pixel size of the panel
    /// (`w * scale`) so the GUI can display HiDPI screens correctly.
    #[serde(default = "default_scale")]
    pub scale: f32,
}

fn default_is_local() -> bool {
    true
}

fn default_scale() -> f32 {
    1.0
}

impl Screen {
    /// Physical pixel size (logical size × UI scale) — what the panel actually renders.
    /// Equals `(w, h)` on non-HiDPI displays and on Windows after DPI awareness.
    pub fn physical_size(&self) -> (u32, u32) {
        (
            (self.w as f32 * self.scale) as u32,
            (self.h as f32 * self.scale) as u32,
        )
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.ox as f64
            && x < (self.ox + self.w as i32) as f64
            && y >= self.oy as f64
            && y < (self.oy + self.h as i32) as f64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Layout {
    pub screens: Vec<Screen>,
}

impl Layout {
    /// Index of the screen that contains the point, if any.
    pub fn screen_at(&self, x: f64, y: f64) -> Option<usize> {
        self.screens.iter().position(|s| s.contains(x, y))
    }

    /// If the point falls in a gap between screens, pull it into the nearest screen and
    /// clamp to that screen's bounds. Used so the virtual cursor never escapes to infinity.
    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        if self.screens.is_empty() {
            return (x, y);
        }
        let mut best = 0usize;
        let mut best_d = f64::MAX;
        for (i, s) in self.screens.iter().enumerate() {
            let cx = s.ox as f64 + s.w as f64 / 2.0;
            let cy = s.oy as f64 + s.h as f64 / 2.0;
            let d = (cx - x).powi(2) + (cy - y).powi(2);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        let s = &self.screens[best];
        let nx = x.clamp(s.ox as f64, (s.ox + s.w as i32) as f64 - 1.0);
        let ny = y.clamp(s.oy as f64, (s.oy + s.h as i32) as f64 - 1.0);
        (nx, ny)
    }

    /// Index of the screen whose name matches (the primary machine's own screen).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.screens.iter().position(|s| s.name == name)
    }

    /// Axis-aligned bounding box (left, top, right, bottom) of every `is_local` screen, in
    /// virtual-desktop coordinates. `None` when there is no local screen (shouldn't happen on a
    /// running primary, but the caller can fall back to a single screen).
    pub fn local_bbox(&self) -> Option<(f64, f64, f64, f64)> {
        let mut it = self.screens.iter().filter(|s| s.is_local);
        let first = it.next()?;
        let (mut l, mut t, mut r, mut b) = (
            first.ox as f64,
            first.oy as f64,
            first.ox as f64 + first.w as f64,
            first.oy as f64 + first.h as f64,
        );
        for s in it {
            l = l.min(s.ox as f64);
            t = t.min(s.oy as f64);
            r = r.max(s.ox as f64 + s.w as f64);
            b = b.max(s.oy as f64 + s.h as f64);
        }
        Some((l, t, r, b))
    }

    /// Ensure a screen named `name` exists, adding it (placed *adjacent* to the right of the
    /// current rightmost screen — no gap) only if absent. Returns true when a new screen was
    /// created. Used to auto-register every secondary that connects, so the client count is
    /// unbounded.
    ///
    /// Placing the new screen flush against the existing extent (instead of leaving a 40px dead
    /// band) is what makes cursor hand-off possible: the virtual cursor advances continuously and
    /// steps straight from the last local pixel into the first remote pixel, so `screen_at` finds
    /// the remote screen instead of a gap that `clamp` would snap back.
    pub fn ensure_screen(&mut self, name: &str, w: u32, h: u32, is_local: bool) -> bool {
        if self.index_of(name).is_some() {
            return false;
        }
        let max_x = self
            .screens
            .iter()
            .map(|s| s.ox + s.w as i32)
            .max()
            .unwrap_or(0);
        self.screens.push(Screen {
            name: name.to_string(),
            ox: if self.screens.is_empty() { 0 } else { max_x },
            oy: 0,
            w,
            h,
            is_local,
            scale: 1.0,
        });
        true
    }

    /// Clone the screen at `idx` with a unique name and an offset to the right, so it can be
    /// re-positioned without disturbing the original.
    pub fn duplicate_screen(&mut self, idx: usize) {
        if let Some(src) = self.screens.get(idx).cloned() {
            let base = src.name.clone();
            let mut n = 1;
            let new_name = loop {
                let cand = format!("{}-copy{}", base, n);
                if self.index_of(&cand).is_none() {
                    break cand;
                }
                n += 1;
            };
            self.screens.push(Screen {
                name: new_name,
                ox: src.ox + src.w as i32 + 40,
                oy: src.oy,
                w: src.w,
                h: src.h,
                is_local: src.is_local,
                scale: src.scale,
            });
        }
    }
}
