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
}

impl Screen {
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

    /// Ensure a screen named `name` exists, adding it (placed to the right of the rightmost
    /// existing screen) only if absent. Returns true when a new screen was created.
    /// Used to auto-register every secondary that connects, so the client count is unbounded.
    pub fn ensure_screen(&mut self, name: &str, w: u32, h: u32) -> bool {
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
            ox: if self.screens.is_empty() { 0 } else { max_x + 40 },
            oy: 0,
            w,
            h,
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
            });
        }
    }
}
