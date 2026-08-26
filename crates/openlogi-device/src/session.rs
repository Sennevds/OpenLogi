//! Long-running device sessions.
//!
//! Each session holds one HID++ channel open for one device, diverts the
//! controls it needs, streams events until stopped, and restores the device's
//! native behavior on the way out: [`gesture`] and [`keyboard`] capture
//! control presses for the agent to dispatch, [`host_switch`] follows a
//! keyboard's host keys to switch its linked pointing devices.
//!
//! The Craft crown (`crown`) has no session of its own: it is armed inside
//! [`keyboard`]'s, which already holds that keyboard's one channel.

mod crown;
pub mod gesture;
pub mod host_switch;
pub mod keyboard;
