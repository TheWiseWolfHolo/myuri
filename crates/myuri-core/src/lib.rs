//! Platform-independent logic for Myuri: the right-button hold gesture,
//! configuration, hotkey parsing and assistant matching. Nothing here touches
//! Win32 or the UI, so `cargo test -p myuri-core` runs in seconds.

pub mod brand;
pub mod catalog;
pub mod config;
pub mod edit;
pub mod gesture;
pub mod hotkey;
pub mod image;
pub mod llm;
pub mod matching;
pub mod speech;
pub mod uistate;
