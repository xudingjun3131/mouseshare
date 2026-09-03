//! Wire protocol shared between the primary (server) and secondary (client) machines.
//!
//! Frames are: `[u32 LE length][JSON(Message)]` sent over a TCP stream.

use rdev::{Button as RdevButton, Key};
use serde::{Deserialize, Serialize};

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
    /// Absolute position *inside the target screen* (already translated to the peer's origin).
    MouseMove { x: f64, y: f64 },
    MouseDown { button: MsButton },
    MouseUp { button: MsButton },
    /// Wheel deltas (sign conventions follow rdev: dy > 0 scrolls down).
    Wheel { dx: i64, dy: i64 },
    /// Physical key (QWERTY layout) so it maps consistently across machines.
    KeyDown { key: Key },
    KeyUp { key: Key },
}

/// Top-level message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Sent immediately after connecting so the hub knows the peer's name + screen size.
    Hello { name: String, width: u32, height: u32 },
    /// An input event destined for a specific screen (routed by the hub / applied by the client).
    Input(InputEvent),
    /// Clipboard contents (broadcast, loop-suppressed on the receiving side).
    Clipboard { text: String },
    /// Hint: the cursor just entered a secondary screen.
    EnterScreen,
    /// Hint: the cursor just left a secondary screen.
    LeaveScreen,
    /// Keep-alive.
    Ping,
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
