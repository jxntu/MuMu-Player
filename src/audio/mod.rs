//! Audio playback subsystem.
//!
//! The module is split into small pieces with clear responsibilities:
//!
//! * `decoder` – turns an encoded file into interleaved `f32` PCM samples.
//! * `buffer`  – the PCM buffer that connects the decode worker to the audio
//!   callback, including the pause / cancel semantics.
//! * `output`  – owns the CPAL stream and wires the buffer to a device.
//! * `player`  – the state machine, the session lifecycle and the decode worker.
//! * `state`   – the `PlaybackState` enum.
//!
//! Nothing in here knows about playlists, metadata databases or the TUI; those
//! are future milestones.

mod buffer;
mod decoder;
mod output;
mod player;
mod state;

#[cfg(test)]
mod test_audio;

pub use decoder::TrackInfo;
pub use output::OutputInfo;
pub use player::Player;
pub use state::PlaybackState;
