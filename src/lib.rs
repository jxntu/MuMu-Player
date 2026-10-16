//! Playback core of **linux-player**, a terminal music player for Linux.
//!
//! The library holds everything that is not the command line front end:
//!
//! ```text
//! audio file -> symphonia decode -> PCM buffer -> cpal -> default device
//! ```
//!
//! [`audio::Player`] is the entry point. It owns the playback state machine and
//! one playback session at a time, and it is free of any output of its own, so a
//! future TUI can drive it just as the `linux-player` binary does.
//!
//! Milestone M2 covers start / pause / resume / stop, correct end-of-stream
//! handling and resource cleanup. Playlists, metadata, seek and volume control
//! come later.

pub mod audio;
