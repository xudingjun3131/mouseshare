//! Wire protocol shared between the primary (server) and secondary (client) machines.
//!
//! Frames are: `[u32 LE length][JSON(Message)]` sent over a TCP stream.
//!
//! ## Coordinate convention
//!
//! Mouse motion is forwarded as a **relative delta** (`MouseMotion { dx, dy }`), never as an
//! absolute position. This is the single most important design decision and the one the old
//! `rdev::listen`-based build got wrong:
//!
//! * Relative deltas need no agreement about screen geometry, DPI or scaling between the two
//!   machines — the receiver just accumulates them against its own real cursor, so a HiDPI
//!   primary driving a non-HiDPI secondary (or vice-versa) never drifts.
//! * The capture side *grabs* the event (macOS `CGEventTap` returning `Drop`) so the delta
//!   stream keeps flowing even when the physical cursor is pinned against a display edge —
//!   the OS never clamps a motion that it never receives, which is exactly why the old
//!   observer-based code needed the treadmill/edge-rest band-aids this protocol makes obsolete.

use crate::layout::Side;
use rdev::{Button as RdevButton, Key};
use serde::{Deserialize, Serialize};

/// Default UI scale for a peer that never reports one (see `Message::Hello`).
fn default_hello_scale() -> f32 {
    1.0
}

/// Our own, serializable mouse-button enum (mirrors `rdev::Button`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MsButton {
    Left,
    Middle,
    Right,
    Other(u8),
}

/// Everything that can be forwarded as an input event to another machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative mouse motion, **already normalised into the receiver's coordinate space**.
    /// The primary multiplies its own OS delta by `own_scale / receiver_scale` before sending
    /// (see `control::enter_forwarding`), so a Retina primary drives a 1x secondary at the same
    /// physical cursor speed instead of half of it. `dx`/`dy` are signed pixel deltas (macOS
    /// `kCGMouseEventDeltaX/Y`, which may be fractional); the receiver adds them to its current
    /// cursor position and clamps to its own screen.
    MouseMotion { dx: f64, dy: f64 },
    MouseDown { button: MsButton },
    MouseUp { button: MsButton },
    /// Wheel deltas (sign conventions follow rdev: dy > 0 scrolls down).
    Wheel { dx: i64, dy: i64 },
    /// Physical key (QWERTY layout) so it maps consistently across machines.
    KeyDown { key: Key },
    KeyUp { key: Key },
}

/// One file inside a cross-machine clipboard copy.
///
/// `path` is relative to the copy root so nested folders survive the trip
/// (e.g. `report.pdf`, `assets/logo.png`). Directories are implicit: they are recreated on
/// the receiving side from the files they contain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

/// Payload chunk size for file transfer: 256 KiB of raw bytes (≈350 KB once base64-encoded).
/// Large enough that a 10 MB copy is only 40 frames, small enough that a chunk never blocks
/// the input stream for a noticeable time.
pub const FILE_CHUNK: usize = 256 * 1024;

/// Hard cap on one file copy (files + total bytes). Anything larger is skipped with a
/// warning rather than buffering hundreds of megabytes in memory on both machines.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Top-level message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Sent immediately after connecting so the hub knows the peer's name + screen size.
    Hello {
        name: String,
        width: u32,
        height: u32,
        /// UI scale factor of the reporting machine's **coordinate space** (2.0 on a Retina Mac,
        /// 1.0 on a DPI-aware Windows/Linux box). The primary needs it to convert its own logical
        /// mouse deltas into the secondary's units — otherwise a Retina primary drives a 1x
        /// secondary at half speed. `#[serde(default)]` keeps peers that predate the field working.
        #[serde(default = "default_hello_scale")]
        scale: f32,
    },
    /// An input event destined for the machine that currently has control (routed by the hub
    /// / applied by the client).
    Input(InputEvent),
    /// Clipboard contents (broadcast, loop-suppressed on the receiving side).
    Clipboard { text: String },
    /// The full screen layout, pushed by the primary to every secondary so all machines draw
    /// the same map (including the primary's own screen and every peer's position). Without
    /// this a secondary only ever sees its own local `config.layout` and never learns about the
    /// primary's display.
    Layout { layout: crate::layout::Layout },
    /// Hint: the cursor just entered a secondary screen. `side` is the edge of the *controlling*
    /// machine that the secondary sits beyond (so the secondary seeds its own cursor at the
    /// opposite edge); `fx`/`fy` are the fractional position along that edge where the cursor
    /// crossed (0..1), used to align entry vertically/horizontally.
    EnterScreen { side: Side, fx: f64, fy: f64 },
    /// Hint: the cursor just left a secondary screen and control returned to the primary.
    LeaveScreen,
    /// Keep-alive.
    Ping,
    /// Hotkey (default ScrollLock) pressed on either machine: the primary rotates control to the
    /// next machine in the layout (primary as one machine, then each secondary). A secondary sends
    /// this to the primary; the primary also handles its own local ScrollLock press.
    Hotkey,
    /// The secondary asks the primary for control back: its virtual cursor was pushed back across
    /// the shared edge toward the primary. The primary leaves forwarding and resumes local control
    /// (unlike `Hotkey`, this always returns to the primary, never rotates to another machine).
    ReturnControl,
    /// The sender just copied one or more **files**. `entries` is the manifest (relative path +
    /// size); the bytes follow as a stream of `FileChunk` frames and the copy is closed by
    /// `FileEnd`. `token` ties the three together so two machines copying at the same time
    /// cannot interleave into one corrupted transfer.
    ClipboardFiles {
        token: u64,
        entries: Vec<FileEntry>,
    },
    /// A base64 chunk of file data for `token`.
    FileChunk { token: u64, seq: u64, data: String },
    /// End of the file transfer for `token`.
    FileEnd { token: u64 },
}

impl MsButton {
    pub fn from_rdev(b: RdevButton) -> MsButton {
        match b {
            RdevButton::Left => MsButton::Left,
            RdevButton::Middle => MsButton::Middle,
            RdevButton::Right => MsButton::Right,
            RdevButton::Unknown(n) => MsButton::Other(n),
        }
    }

    pub fn to_rdev(self) -> RdevButton {
        match self {
            MsButton::Left => RdevButton::Left,
            MsButton::Middle => RdevButton::Middle,
            MsButton::Right => RdevButton::Right,
            MsButton::Other(n) => RdevButton::Unknown(n),
        }
    }
}
