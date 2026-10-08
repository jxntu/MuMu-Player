//! Audio playback subsystem.
//!
//! The module is deliberately split into three small pieces with clear
//! responsibilities:
//!
//! * [`decoder`] – turns an encoded file into interleaved `f32` PCM samples.
//! * [`output`] – owns the CPAL stream and the bounded PCM buffer that feeds it.
//! * [`player`] – glues the two together and owns the playback loop.
//!
//! Nothing in here knows about playlists, metadata databases or the TUI; those
//! are future milestones.

mod decoder;
mod output;
mod player;

pub use player::Player;
