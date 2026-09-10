//! Screen layout model: a set of rectangles placed in a virtual desktop coordinate space.
//! Each rectangle represents one physical machine's screen.

use serde::{Deserialize, Serialize};

/// Which side of a machine's local bounding box a neighbour screen is attached beyond.
/// `Right` means "the neighbour sits to the right of this machine", etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Right,
    Left,
    Top,
    Bottom,
}

impl Side {
    /// The edge of the *other* machine that this screen's cursor enters from. If a secondary is
    /// to the `Right` of the primary, the cursor reaches it by crossing the primary's right edge
    /// and appears on the secondary's `Left` edge — so the secondary seeds its virtual cursor on
    /// its own opposite side.
    pub fn opposite(self) -> Side {
        match self {
            Side::Right => Side::Left,
            Side::Left => Side::Right,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screen {
    /// Unique name of this *panel*. For a local display this is the machine name (or
    /// `"<machine> #2"`), for a remote panel it is the name the owning machine gave it.
    pub name: String,
    /// The machine this panel physically belongs to (`Config.name` of that host). Equals `name`
    /// for a single-display machine, which is why the field defaults to empty and
    /// [`Screen::host`] falls back to `name`.
    ///
    /// This is the field that makes "one computer, several monitors" work: input is routed to a
    /// *machine* (one TCP peer) but crossing is decided per *panel*, so a peer with two monitors
    /// is two draggable tiles that both forward to the same socket.
    #[serde(default)]
    pub host: String,
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
    /// The machine that owns this panel (see [`Screen::host`]).
    pub fn host(&self) -> &str {
        if self.host.is_empty() {
            &self.name
        } else {
            &self.host
        }
    }

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

/// One display as reported by the machine that owns it, in that machine's **own** coordinate
/// space (the origin is that machine's local virtual-desktop origin, not the shared one).
///
/// A peer sends its full list of these in `Message::Hello`, which is what lets the hub model a
/// two-monitor Windows box as two separate tiles — and, crucially, know about the dead space in
/// an L-shaped arrangement so it never hands the cursor off into a region with no panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSpec {
    pub name: String,
    pub ox: i32,
    pub oy: i32,
    pub w: u32,
    pub h: u32,
    #[serde(default = "default_scale")]
    pub scale: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Layout {
    pub screens: Vec<Screen>,
}

impl Layout {
    /// Every panel belonging to `host`, in layout order.
    pub fn panels_of<'a>(&'a self, host: &'a str) -> impl Iterator<Item = &'a Screen> + 'a {
        self.screens.iter().filter(move |s| s.host() == host)
    }

    /// Axis-aligned bounding box of one machine's panels. `None` when that machine has no panel
    /// in the layout (e.g. a peer that has not connected yet).
    pub fn host_bbox(&self, host: &str) -> Option<(f64, f64, f64, f64)> {
        let mut it = self.panels_of(host);
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

    /// Top-left corner of a machine's panel group — the anchor that survives a refresh, so the
    /// user's placement of the machine is preserved when it reconnects or changes resolution.
    pub fn machine_origin(&self, host: &str) -> Option<(i32, i32)> {
        let b = self.host_bbox(host)?;
        Some((b.0 as i32, b.1 as i32))
    }

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

    /// UI scale factor of the screen named `name` (1.0 when unknown).
    pub fn scale_of(&self, name: &str) -> f32 {
        self.screens
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.scale)
            .unwrap_or(1.0)
    }

    /// UI scale of the machine `host`'s coordinate space. A machine may mix a Retina panel with a
    /// 1x one; the reported `Hello` scale (and therefore the forwarded-delta conversion) is a
    /// single number per machine, so this returns the first panel's scale — which is the same
    /// value the peer advertises.
    pub fn scale_of_host(&self, host: &str) -> f32 {
        self.panels_of(host).next().map(|s| s.scale).unwrap_or(1.0)
    }

    /// UI scale factor of the *local* screen under `loc`, falling back to the first local screen
    /// and then to 1.0. Used to normalise forwarded mouse deltas: the cursor may sit on a Retina
    /// panel (2.0) or on an external 1x monitor, and each needs a different conversion.
    pub fn local_scale_at(&self, loc: Option<(f64, f64)>) -> f32 {
        let locals: Vec<&Screen> = self.screens.iter().filter(|s| s.is_local).collect();
        if locals.is_empty() {
            return 1.0;
        }
        if let Some((x, y)) = loc {
            if let Some(s) = locals.iter().find(|s| s.contains(x, y)) {
                return s.scale;
            }
        }
        locals[0].scale
    }

    /// Register (or refresh) **every** panel of the machine `host` from the list it reported.
    ///
    /// Placement rules, and why they are what they are:
    ///
    /// * A machine that is new to the layout is placed *flush* against the current right edge, its
    ///   panels keeping the relative offsets they have on their own machine (so an L-shaped or
    ///   stacked arrangement is modelled faithfully — otherwise crossing could fire into a region
    ///   with no panel).
    /// * A machine that is already known keeps its **anchor** (the top-left of its panel group) and
    ///   only its panel sizes / offsets are refreshed. That is what makes the user's drag stick
    ///   across a peer reconnect — and it is why the anchor, not the individual tiles, is the unit
    ///   of persistence.
    ///
    /// Returns `true` when the layout changed in a way worth broadcasting.
    pub fn ensure_host(&mut self, host: &str, panels: &[PanelSpec]) -> bool {
        if panels.is_empty() {
            return false;
        }
        let known = self.screens.iter().any(|s| s.host() == host);
        // `Hello` reports panel offsets normalised against the peer's own bounding-box origin, so
        // the minimum is already 0 and these are no-ops. Subtracting it anyway keeps the anchor
        // arithmetic below correct for *any* caller, instead of silently depending on that
        // guarantee: without it, a panel list whose offsets start below zero would be placed
        // partly underneath the hub (new host) or drift further left on every refresh (known host).
        let min_ox = panels.iter().map(|p| p.ox).min().unwrap_or(0);
        let min_oy = panels.iter().map(|p| p.oy).min().unwrap_or(0);
        if !known {
            // Flush against the right edge of everything already placed, top-aligned with the
            // local screens when there are any (so a freshly connected peer is reachable from the
            // edge the user is most likely sitting on).
            let max_x = self
                .screens
                .iter()
                .map(|s| s.ox + s.w as i32)
                .max()
                .unwrap_or(0);
            let top = self
                .local_bbox()
                .map(|b| b.1 as i32)
                .or_else(|| self.screens.iter().map(|s| s.oy).min())
                .unwrap_or(0);
            for p in panels {
                self.screens.push(Screen {
                    name: p.name.clone(),
                    host: host.to_string(),
                    ox: max_x + (p.ox - min_ox),
                    oy: top + (p.oy - min_oy),
                    w: p.w,
                    h: p.h,
                    is_local: false,
                    scale: p.scale,
                });
            }
            return true;
        }

        // Already known: re-anchor the group and refresh geometry.
        let (ax, ay) = self.machine_origin(host).unwrap_or((0, 0));
        let mut changed = false;
        for p in panels {
            match self
                .screens
                .iter_mut()
                .find(|s| s.name == p.name && s.host() == host)
            {
                Some(s) => {
                    if s.w != p.w || s.h != p.h || s.scale != p.scale {
                        changed = true;
                    }
                    s.w = p.w;
                    s.h = p.h;
                    s.scale = p.scale;
                    s.ox = ax + (p.ox - min_ox);
                    s.oy = ay + (p.oy - min_oy);
                }
                None => {
                    // A monitor that was not there last time (or a renamed one).
                    let s = Screen {
                        name: p.name.clone(),
                        host: host.to_string(),
                        ox: ax + (p.ox - min_ox),
                        oy: ay + (p.oy - min_oy),
                        w: p.w,
                        h: p.h,
                        is_local: false,
                        scale: p.scale,
                    };
                    self.screens.push(s);
                    changed = true;
                }
            }
        }
        // Drop panels this machine no longer reports (unplugged monitor).
        let before = self.screens.len();
        let mine: Vec<String> = panels.iter().map(|p| p.name.clone()).collect();
        self.screens
            .retain(|s| s.host() != host || mine.contains(&s.name));
        if self.screens.len() != before {
            changed = true;
        }
        changed
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
                host: src.host.clone(),
                ox: src.ox + src.w as i32 + 40,
                oy: src.oy,
                w: src.w,
                h: src.h,
                is_local: src.is_local,
                scale: src.scale,
            });
        }
    }

    /// Move every panel of `host` by `(dx, dy)` — dragging one tile of a multi-monitor machine
    /// moves the whole machine, because the arrangement *inside* a machine is a fact of its own
    /// OS, not a choice the hub gets to make.
    pub fn move_host(&mut self, host: &str, dx: i32, dy: i32) {
        for s in self.screens.iter_mut().filter(|s| s.host() == host) {
            s.ox += dx;
            s.oy += dy;
        }
    }

    /// Fill in a missing [`Screen::host`] on panels saved before the field existed.
    ///
    /// Back then a machine could only contribute one display, so a panel's *name* was its machine
    /// name — and a multi-display machine's extra panels were named `<machine> #N` (see
    /// `main::name_panels`). Recovering the machine from the name is therefore exact, not a guess.
    ///
    /// Leaving the field empty is not harmless: `Screen::host` falls back to `name`, so a restored
    /// `"X #2"` panel claims to be owned by a machine literally called `"X #2"`. When peer `X`
    /// reconnects, `ensure_host` looks for `name == "X #2" && host == "X"`, finds nothing, and
    /// **adds a second copy of that panel** — two tiles with one egui id (dragging one moves the
    /// other) and two overlapping crossing candidates. Normalising on load closes that hole.
    pub fn normalize_hosts(&mut self) {
        for s in self.screens.iter_mut() {
            if !s.host.is_empty() {
                continue;
            }
            s.host = match s.name.rfind(" #") {
                // `" #"` followed by digits only, and at least one digit present.
                Some(i)
                    if i + 2 < s.name.len()
                        && s.name[i + 2..].chars().all(|c| c.is_ascii_digit()) =>
                {
                    s.name[..i].to_string()
                }
                _ => s.name.clone(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str, ox: i32, w: u32) -> Screen {
        Screen {
            name: name.into(),
            host: name.into(),
            ox,
            oy: 0,
            w,
            h: 1080,
            is_local: true,
            scale: 1.0,
        }
    }

    /// A config written before `Screen::host` existed has an empty `host` on every panel. Left
    /// alone, `Screen::host` falls back to `name`, so a `"X #2"` panel claims to belong to a
    /// machine literally called `"X #2"`.
    #[test]
    fn normalize_hosts_recovers_the_machine_from_the_panel_name() {
        let mut l = Layout {
            screens: vec![
                Screen {
                    host: String::new(),
                    ..local("mac", 0, 1470)
                },
                Screen {
                    name: "mac #2".into(),
                    host: String::new(),
                    ox: 1470,
                    oy: -124,
                    w: 1920,
                    h: 1080,
                    is_local: true,
                    scale: 2.0,
                },
                Screen {
                    name: "pc".into(),
                    host: String::new(),
                    ox: 3390,
                    oy: 0,
                    w: 1920,
                    h: 1080,
                    is_local: false,
                    scale: 1.0,
                },
            ],
        };
        l.normalize_hosts();
        let hosts: Vec<&str> = l.screens.iter().map(|s| s.host()).collect();
        assert_eq!(hosts, ["mac", "mac", "pc"]);

        // Idempotent — and re-running it must not eat part of a name.
        let before = hosts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        l.normalize_hosts();
        let after: Vec<String> = l.screens.iter().map(|s| s.host().to_string()).collect();
        assert_eq!(before, after);
    }

    /// Only a purely numeric `#N` suffix is a panel index. A name that merely contains a hash, or
    /// has text after it, must survive intact.
    #[test]
    fn normalize_hosts_only_strips_a_numeric_panel_suffix() {
        let mut l = Layout {
            screens: ["adam #2", "mac #2x", "box #", "plain", "odd #1 #2"]
                .iter()
                .enumerate()
                .map(|(i, n)| Screen {
                    name: (*n).into(),
                    host: String::new(),
                    ox: i as i32 * 100,
                    ..local(*n, 0, 100)
                })
                .collect(),
        };
        l.normalize_hosts();
        let hosts: Vec<&str> = l.screens.iter().map(|s| s.host()).collect();
        assert_eq!(hosts, ["adam", "mac #2x", "box #", "plain", "odd #1"]);
    }

    /// `Hello` reports panel offsets normalised so the minimum is 0, but `ensure_host` must not
    /// silently depend on that. With a list that starts below zero the old `max_x + p.ox` placed a
    /// machine *partly on top of the hub* — the two machines would overlap on the canvas and
    /// `predict_cross` would find spans that cannot physically exist.
    #[test]
    fn ensure_host_places_a_new_machine_flush_even_with_negative_offsets() {
        let mut l = Layout {
            screens: vec![local("hub", 0, 1920)],
        };
        // A peer with a monitor to the left of the one it calls its main display.
        let panels = [
            PanelSpec {
                name: "pc".into(),
                ox: -1920,
                oy: -200,
                w: 1920,
                h: 1080,
                scale: 1.0,
            },
            PanelSpec {
                name: "pc #2".into(),
                ox: 0,
                oy: 0,
                w: 1920,
                h: 1080,
                scale: 1.0,
            },
        ];
        assert!(l.ensure_host("pc", &panels));

        let (bl, bt, br, bb) = l.host_bbox("pc").unwrap();
        assert_eq!(
            bl, 1920.0,
            "the group's left edge must sit on the hub's right edge"
        );
        assert_eq!(
            br - bl,
            3840.0,
            "the group keeps the peer's own arrangement"
        );
        assert_eq!(bb - bt, 1280.0, "including the vertical spread");
        for s in l.panels_of("pc") {
            assert!(s.ox >= 1920, "{} overlaps the hub (ox={})", s.name, s.ox);
        }
    }

    /// The anchor is the group's bounding box, so a refresh must reproduce it exactly. When the
    /// anchor was instead used as "the offset of whatever panel has offset 0", each hello shifted
    /// the machine by the group's extent again — it marched off the canvas.
    #[test]
    fn ensure_host_reanchor_is_stable_across_refreshes() {
        let mut l = Layout {
            screens: vec![local("hub", 0, 1920)],
        };
        let panels = [
            PanelSpec {
                name: "pc".into(),
                ox: -1920,
                oy: 0,
                w: 1920,
                h: 1080,
                scale: 1.0,
            },
            PanelSpec {
                name: "pc #2".into(),
                ox: 0,
                oy: 0,
                w: 2560,
                h: 1440,
                scale: 2.0,
            },
        ];
        l.ensure_host("pc", &panels);
        let first: Vec<(String, i32, i32)> = l
            .panels_of("pc")
            .map(|s| (s.name.clone(), s.ox, s.oy))
            .collect();

        // Simulate the 400 ms layout snapshots / repeated hellos a live peer produces.
        for _ in 0..5 {
            l.ensure_host("pc", &panels);
        }
        let after: Vec<(String, i32, i32)> = l
            .panels_of("pc")
            .map(|s| (s.name.clone(), s.ox, s.oy))
            .collect();
        assert_eq!(first, after, "a refresh must not move the machine");
        assert_eq!(first.len(), 2);
    }

    /// A user's placement has to survive the peer changing resolution, and a monitor that is no
    /// longer reported must be dropped rather than left as a phantom panel.
    #[test]
    fn ensure_host_refreshes_geometry_and_drops_unplugged_panels() {
        let mut l = Layout {
            screens: vec![local("hub", 0, 1920)],
        };
        let two = [
            PanelSpec {
                name: "pc".into(),
                ox: 0,
                oy: 0,
                w: 1920,
                h: 1080,
                scale: 1.0,
            },
            PanelSpec {
                name: "pc #2".into(),
                ox: 1920,
                oy: 0,
                w: 1920,
                h: 1080,
                scale: 1.0,
            },
        ];
        l.ensure_host("pc", &two);
        // The user parks the machine above the hub.
        l.move_host("pc", -1920, -1080);
        let anchored = l.machine_origin("pc").unwrap();

        // Monitor #2 is unplugged, and monitor #1 now runs at a different size.
        let one = [PanelSpec {
            name: "pc".into(),
            ox: 0,
            oy: 0,
            w: 2560,
            h: 1440,
            scale: 2.0,
        }];
        l.ensure_host("pc", &one);

        assert_eq!(
            l.panels_of("pc").count(),
            1,
            "the unplugged panel must be dropped"
        );
        assert_eq!(
            l.machine_origin("pc"),
            Some(anchored),
            "placement is preserved"
        );
        let s = l.panels_of("pc").next().unwrap();
        assert_eq!(
            (s.w, s.h, s.scale),
            (2560, 1440, 2.0),
            "geometry is refreshed"
        );
    }
}
