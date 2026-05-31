//! Library face of the `screenshare` crate.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around these modules.
//! The library target exists so integration tests under `tests/` can import
//! pure functions (codec, fallback wire format, capture preset math,
//! signaling URL builder) without dragging in egui, the tokio runtime, or
//! the WebRTC stack. Keep this file declarative — no logic.

pub mod app;
pub mod capture;
pub mod fallback;
pub mod render;
pub mod signaling;
pub mod webrtc_client;
