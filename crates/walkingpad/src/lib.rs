//! KingSmith WalkingPad R1 Pro: wire protocol, BLE transport abstraction and
//! the session state machine that enforces the safety rules (design §4, §5,
//! §7). No Tauri dependency, so all of it runs under `cargo test`.
//!
//! - [`protocol`] is the only module that knows bytes. Pure, no I/O.
//! - [`transport`] is the BLE seam: the app implements it with
//!   `tauri-plugin-blec`; [`fake`] implements it with a scripted pad.
//! - [`session`] is the only module that decides when to send.

pub mod protocol;
pub mod session;
pub mod transport;

#[cfg(any(test, feature = "fake"))]
pub mod fake;
