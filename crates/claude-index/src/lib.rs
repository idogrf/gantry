//! Discovers Claude Code sessions from Claude's config directory (read-only): transcripts for history,
//! pid files for live instances. Formats are undocumented, so every reader is defensive.

pub mod config;
pub mod containers;
pub mod discovery;
pub mod host;
pub mod index;
pub mod live;
pub mod process;
pub mod search;
pub mod transcript;
pub mod watch;
