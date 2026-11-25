//! The TapDrop protocol, independent of any user interface or hardware.
//!
//! Frames and the encrypted channel, pairing and its six-digit code, file names, and the light key
//! that decides whether a tap counts. Everything here is plain Rust: the host and its tests use it
//! without GTK, without sensors, and without a phone.

pub mod crypto;
pub mod files;
pub mod lightkey;
pub mod sensors;
pub mod wire;
