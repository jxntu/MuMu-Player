//! Playback states.
//!
//! The player has exactly one state at a time; there is deliberately no set of
//! independent `is_playing` / `is_paused` booleans that could drift apart.

use std::fmt;

/// What the player is currently doing.
///
/// The distinction between [`PlaybackState::Stopped`] and
/// [`PlaybackState::Finished`] matters:
///
/// * `Finished` means the file reached its natural end and every buffered sample
///   was actually played. The session is still alive, so the player still knows
///   *what* finished.
/// * `Stopped` means nothing is playing: either the user stopped playback, or no
///   session has been started yet. The session's resources have been released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    /// No audio is playing. The initial state and the state after [`stop`].
    ///
    /// [`stop`]: crate::audio::Player::stop
    Stopped,
    /// Audio is being played through the audio device.
    Playing,
    /// Playback is suspended: the audio device outputs silence and the buffer
    /// keeps every sample that has not been played yet.
    Paused,
    /// The file reached its natural end and everything buffered was played.
    Finished,
}

impl PlaybackState {
    /// `true` while a session can still be paused, resumed or observed.
    ///
    /// `Finished` and `Stopped` are terminal: the only way out of them is a new
    /// [`play`](crate::audio::Player::play) call.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Playing | Self::Paused)
    }

    /// `true` when no session is running and no audio will be produced.
    pub fn is_terminal(self) -> bool {
        !self.is_active()
    }
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Stopped => "stopped",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Finished => "finished",
        };
        f.write_str(name)
    }
}
