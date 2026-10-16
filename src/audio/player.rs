//! Playback control: the state machine, the session lifecycle and the decode
//! worker.
//!
//! A [`Player`] owns at most one *session*: a decode worker thread that fills a
//! PCM buffer, plus the audio stream that drains it.
//!
//! ```text
//! Player  (state machine, session lifecycle)
//!    └── decode worker  --push-->  PcmBuffer  --callback-->  audio device
//! ```
//!
//! Everything that can block (decoding, waiting for buffer space, waiting for the
//! device to drain) happens on the worker thread — never in the audio callback and
//! never on the caller's thread. The caller only writes flags, which is why
//! [`Player::pause`], [`Player::resume`] and [`Player::stop`] return immediately
//! and can be called while playback is going on.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::buffer::PcmBuffer;
use super::decoder::{Decoder, TrackInfo};
use super::output::{AudioOutput, OutputInfo};
use super::state::PlaybackState;

/// How long [`Player::wait`] sleeps between two state checks.
const WAIT_POLL: Duration = Duration::from_millis(2);

/// Where a session's PCM is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// The system default audio device.
    Device,
    /// Nowhere: the samples stay in the buffer for the caller to consume.
    ///
    /// This exists so the state machine can be exercised without a sound card.
    /// The worker, the buffer and both loops are identical to [`Backend::Device`],
    /// so the only thing a detached test does not cover is CPAL itself.
    #[cfg(test)]
    Detached,
}

/// How a session ended, as published by its worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// The stream reached its natural end and everything buffered was played.
    Finished,
    /// `stop` (or a new `play`) asked the session to end early.
    Stopped,
    /// The session ended because of an error; see [`SessionOutcome::take_error`].
    Failed,
}

/// Terminal result of a session, published by its worker thread.
///
/// `ended()` is `None` for as long as the worker is still running.
#[derive(Debug, Default)]
struct SessionOutcome {
    /// `None` while the worker runs.
    ended: Mutex<Option<Ended>>,
    /// Root cause, only set when `ended()` is `Some(Ended::Failed)`.
    error: Mutex<Option<anyhow::Error>>,
}

impl SessionOutcome {
    /// Publishes a successful end of session.
    fn finish(&self, ended: Ended) {
        *lock(&self.ended) = Some(ended);
    }

    /// Publishes a failure. Never conflated with a natural end of stream.
    fn fail(&self, error: anyhow::Error) {
        *lock(&self.error) = Some(error);
        *lock(&self.ended) = Some(Ended::Failed);
    }

    fn ended(&self) -> Option<Ended> {
        *lock(&self.ended)
    }

    fn take_error(&self) -> Option<anyhow::Error> {
        lock(&self.error).take()
    }
}

/// One playback session.
struct Session {
    /// The file being played, kept so the caller can display it.
    info: TrackInfo,
    /// Buffer shared with the worker and (for [`Backend::Device`]) the callback.
    buffer: Arc<PcmBuffer>,
    /// Output stream. Dropping it stops the callback and releases the device.
    /// `None` for [`Backend::Detached`].
    output: Option<AudioOutput>,
    /// The decode worker; `None` once it has been joined.
    worker: Option<JoinHandle<()>>,
    /// Terminal result, written by the worker.
    outcome: Arc<SessionOutcome>,
}

impl Session {
    /// The state this session is in right now.
    fn state(&self) -> PlaybackState {
        match self.outcome.ended() {
            Some(Ended::Finished) => PlaybackState::Finished,
            // Stopping and failing are both terminal. They are still never
            // conflated: a failure is handed out by `Player::take_error` and is
            // never reported as a normal end of stream.
            Some(Ended::Stopped | Ended::Failed) => PlaybackState::Stopped,
            None if self.buffer.is_paused() => PlaybackState::Paused,
            None => PlaybackState::Playing,
        }
    }

    /// Ends the session: ask the worker to leave its loops, wait for it, then
    /// release the stream.
    fn shutdown(mut self) {
        self.buffer.cancel();
        if let Some(worker) = self.worker.take() {
            // The worker only ever blocks in `wait_for_space` /
            // `wait_until_drained`, and `cancel` releases both, so this join is
            // bounded. It is never called from the audio callback.
            let _ = worker.join();
        }
        drop(self.output);
    }
}

/// Plays one file at a time and exposes playback control.
pub struct Player {
    backend: Backend,
    /// The active session, if any. `None` means nothing is playing.
    session: Option<Session>,
    /// Failure of a session that has already been released, kept until the
    /// caller collects it.
    error: Option<anyhow::Error>,
}

impl Player {
    /// Creates a player that plays through the system default audio device.
    pub fn new() -> Self {
        Self::with_backend(Backend::Device)
    }

    /// A player whose PCM is consumed by the caller instead of an audio device.
    #[cfg(test)]
    fn detached() -> Self {
        Self::with_backend(Backend::Detached)
    }

    fn with_backend(backend: Backend) -> Self {
        Self {
            backend,
            session: None,
            error: None,
        }
    }

    /// Starts playing `path` in the background.
    ///
    /// Returns as soon as the session is running; use [`Player::wait`] to block
    /// until it ends.
    ///
    /// The file is opened and probed *before* the current session is torn down, so
    /// a path that cannot be played never interrupts what is already playing. Once
    /// the new file has been opened, the old session is stopped and a brand new
    /// buffer is created, so a sample of the previous file can never reach the new
    /// session.
    ///
    /// Playing also clears the error of the previous session; collect it with
    /// [`Player::take_error`] first if it matters.
    pub fn play(&mut self, path: &Path) -> Result<()> {
        let decoder = Decoder::open(path)?;
        let info = decoder.info().clone();
        let channels = u16::try_from(info.channels).with_context(|| {
            format!(
                "cannot play {}: the file has {} channels, more than the audio \
                 output supports",
                path.display(),
                info.channels
            )
        })?;

        // From here on the current session is replaced, whatever happens next.
        self.teardown();
        self.error = None;

        let (buffer, output) = match self.backend {
            Backend::Device => {
                let output = AudioOutput::new(info.sample_rate, channels)
                    .with_context(|| format!("cannot play {}", path.display()))?;
                output.start()?;
                (Arc::clone(output.buffer()), Some(output))
            }
            // The test suite's stand-in for the sound card; see [`Backend`].
            #[cfg(test)]
            Backend::Detached => (Arc::new(PcmBuffer::new(info.sample_rate, channels)), None),
        };

        let outcome = Arc::new(SessionOutcome::default());
        let worker = spawn_worker(decoder, Arc::clone(&buffer), Arc::clone(&outcome))?;

        self.session = Some(Session {
            info,
            buffer,
            output,
            worker: Some(worker),
            outcome,
        });
        Ok(())
    }

    /// Suspends playback.
    ///
    /// The audio callback keeps running, but it outputs silence and consumes
    /// nothing, so the playback position does not move and no buffered sample is
    /// lost. [`Player::resume`] continues from exactly where this stopped.
    ///
    /// Idempotent while paused. Returns an error when nothing is playing.
    pub fn pause(&mut self) -> Result<()> {
        self.controllable("pause")?.buffer.set_paused(true);
        Ok(())
    }

    /// Resumes a paused session from the position it was paused at.
    ///
    /// The file is *not* reopened: the decoder, the buffer and the stream are the
    /// ones the session already had.
    ///
    /// Idempotent while playing. Returns an error when nothing is playing.
    pub fn resume(&mut self) -> Result<()> {
        self.controllable("resume")?.buffer.set_paused(false);
        Ok(())
    }

    /// Stops playback and releases the session's resources.
    ///
    /// Idempotent: stopping an idle player succeeds and does nothing. Afterwards
    /// [`Player::play`] can be called again.
    pub fn stop(&mut self) -> Result<()> {
        self.teardown();
        Ok(())
    }

    /// The state the player is in right now.
    pub fn state(&self) -> PlaybackState {
        self.session
            .as_ref()
            .map_or(PlaybackState::Stopped, Session::state)
    }

    /// Blocks until the active session reaches a terminal state, and returns it.
    ///
    /// Returns immediately when the player is idle. A paused session stays paused
    /// until it is resumed, so waiting on one blocks for that long.
    pub fn wait(&self) -> PlaybackState {
        loop {
            let state = self.state();
            if state.is_terminal() {
                return state;
            }
            thread::sleep(WAIT_POLL);
        }
    }

    /// Takes the error that ended the active session, if it failed.
    ///
    /// A failure is never reported as [`PlaybackState::Finished`]: this is how the
    /// two are told apart.
    pub fn take_error(&mut self) -> Option<anyhow::Error> {
        if let Some(session) = &self.session
            && let Some(error) = session.outcome.take_error()
        {
            self.error = Some(error);
        }
        self.error.take()
    }

    /// Format of the file the active session is playing.
    pub fn track_info(&self) -> Option<&TrackInfo> {
        self.session.as_ref().map(|session| &session.info)
    }

    /// How the audio stream of the active session was opened.
    ///
    /// `None` when nothing is playing, or when the session has no device (which
    /// only happens in the test suite).
    pub fn output_info(&self) -> Option<&OutputInfo> {
        self.session
            .as_ref()
            .and_then(|session| session.output.as_ref())
            .map(AudioOutput::info)
    }

    /// Returns the session that accepts playback control, or an error explaining
    /// why the requested operation does not apply right now.
    fn controllable(&self, operation: &str) -> Result<&Session> {
        match &self.session {
            Some(session) if session.state().is_active() => Ok(session),
            Some(_) => bail!("cannot {operation}: playback has already finished"),
            None => bail!("cannot {operation}: nothing is playing"),
        }
    }

    /// Ends the active session, keeping the error it reported (if any).
    fn teardown(&mut self) {
        if let Some(session) = self.session.take() {
            if let Some(error) = session.outcome.take_error() {
                self.error = Some(error);
            }
            session.shutdown();
        }
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Player {
    /// Leaves no worker thread behind and releases the audio device.
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Starts the decode worker of a session.
fn spawn_worker(
    mut decoder: Decoder,
    buffer: Arc<PcmBuffer>,
    outcome: Arc<SessionOutcome>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("linux-player-decode".to_owned())
        .spawn(move || match fill_buffer(&mut decoder, &buffer) {
            Ok(ended) => outcome.finish(ended),
            Err(error) => outcome.fail(error),
        })
        .context("failed to start the decode thread")
}

/// Decodes the whole file into the buffer, then waits until it has been played.
///
/// This is the only place that pulls from the decoder, so it is also the only
/// place that decides how a session ended.
fn fill_buffer(decoder: &mut Decoder, buffer: &Arc<PcmBuffer>) -> Result<Ended> {
    // Produce: decode and queue, waiting whenever the buffer is full. That wait is
    // what bounds memory use, and it is also why pausing freezes the position: a
    // paused callback consumes nothing, so the buffer stays full and the decoder
    // stops making progress.
    loop {
        if buffer.is_cancelled() {
            return Ok(Ended::Stopped);
        }
        if buffer.stream_failed() {
            return Err(stream_failed_error());
        }
        if buffer.free_space() == 0 {
            buffer.wait_for_space();
            continue;
        }
        match decoder.next_chunk()? {
            Some(samples) => buffer.push(samples),
            None => break,
        }
    }

    // The end of the file is not the end of playback: everything already decoded
    // still has to reach the device before the session can be called finished.
    buffer.finish_input();
    loop {
        if buffer.is_cancelled() {
            return Ok(Ended::Stopped);
        }
        if buffer.stream_failed() {
            return Err(stream_failed_error());
        }
        if buffer.is_drained() {
            return Ok(Ended::Finished);
        }
        buffer.wait_until_drained();
    }
}

/// Error used when CPAL reports that the output stream died.
fn stream_failed_error() -> anyhow::Error {
    anyhow!("the audio output stream stopped unexpectedly")
}

/// Locks `mutex`, recovering from a poisoned lock.
///
/// A poisoned lock means another thread panicked while holding it. The data it
/// guards is still consistent, and giving up on playback would be worse.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::test_audio::{TempDir, sample_count, write_wav};
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    /// Sample rate of every fixture.
    const RATE: u32 = 44_100;
    /// Channel count of every fixture.
    const CHANNELS: u16 = 2;
    /// How long a test waits for something to happen before giving up.
    const TIMEOUT: Duration = Duration::from_secs(30);

    // ----------------------------------------------------------------- fixtures

    /// A 0.2 s file of a constant value.
    fn constant_file(dir: &TempDir, name: &str, value: f32) -> std::path::PathBuf {
        let path = dir.join(name);
        write_wav(&path, RATE, CHANNELS, RATE / 5, |_| value).expect("write fixture");
        path
    }

    /// A one second file whose samples ramp from -0.9 up to +0.9.
    ///
    /// A monotone signal is what makes it possible to tell *where* playback
    /// resumed: restarting, skipping or repeating a chunk all show up as a jump.
    fn ramp_file(dir: &TempDir, name: &str) -> std::path::PathBuf {
        let frames = RATE;
        let path = dir.join(name);
        write_wav(&path, RATE, CHANNELS, frames, |frame| {
            -0.9 + 1.8 * (frame as f32 / frames as f32)
        })
        .expect("write fixture");
        path
    }

    /// Waits until `predicate` holds, or gives up after `TIMEOUT`.
    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        predicate()
    }

    // ------------------------------------------------------------ stand-in device

    /// Stands in for the sound card.
    ///
    /// It pulls samples out of the player's buffer with the very same
    /// [`PcmBuffer::fill_from_buffer`] the CPAL callback uses, and remembers what
    /// it "played" so a test can assert on the audio itself. Only the clock is
    /// fake here, which keeps every state machine test independent of a sound
    /// card while still exercising the real consumption path.
    struct FakeDevice {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        played: Arc<Mutex<Vec<f32>>>,
    }

    impl FakeDevice {
        fn start(buffer: Arc<PcmBuffer>, period: Duration) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let played = Arc::new(Mutex::new(Vec::new()));
            // 10 ms worth of frames per period, like a real device callback.
            let block = (RATE as usize / 100) * usize::from(CHANNELS);

            let worker = {
                let stop = Arc::clone(&stop);
                let played = Arc::clone(&played);
                thread::spawn(move || {
                    let mut out = vec![0.0f32; block];
                    while !stop.load(Ordering::Relaxed) {
                        thread::sleep(period);
                        let taken = buffer.fill_from_buffer(&mut out);
                        if taken > 0 {
                            lock(&played).extend_from_slice(&out[..taken]);
                        }
                    }
                })
            };

            Self {
                stop,
                worker: Some(worker),
                played,
            }
        }

        /// Everything this device has played so far.
        fn played(&self) -> Vec<f32> {
            lock(&self.played).clone()
        }

        fn played_len(&self) -> usize {
            lock(&self.played).len()
        }
    }

    impl Drop for FakeDevice {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// Attaches a stand-in device to the player's current session.
    ///
    /// `speed` is how much faster than real time it consumes audio.
    ///
    /// A stand-in device belongs to one session, exactly like a real stream does:
    /// it drains the buffer that existed when it was attached, so a test that
    /// starts a new session has to attach a new one.
    fn attach_device(player: &Player, speed: u32) -> FakeDevice {
        let buffer = player
            .session
            .as_ref()
            .map(|session| Arc::clone(&session.buffer))
            .expect("the player must have an active session");
        FakeDevice::start(buffer, Duration::from_millis(u64::from(10 / speed).max(1)))
    }

    // ------------------------------------------------------- state transitions

    #[test]
    fn play_moves_from_stopped_to_playing() {
        let dir = TempDir::new("play").expect("temp dir");
        let mut player = Player::detached();

        assert_eq!(player.state(), PlaybackState::Stopped);

        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");

        assert_eq!(player.state(), PlaybackState::Playing);
        assert!(player.track_info().is_some());
        assert!(
            player.output_info().is_none(),
            "a detached player has no device"
        );
    }

    #[test]
    fn pause_and_resume_move_between_playing_and_paused() {
        let dir = TempDir::new("pause-state").expect("temp dir");
        let mut player = Player::detached();
        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");

        player.pause().expect("pause");
        assert_eq!(player.state(), PlaybackState::Paused);

        // Pausing twice is idempotent.
        player.pause().expect("pause again");
        assert_eq!(player.state(), PlaybackState::Paused);

        player.resume().expect("resume");
        assert_eq!(player.state(), PlaybackState::Playing);

        // So is resuming twice.
        player.resume().expect("resume again");
        assert_eq!(player.state(), PlaybackState::Playing);
    }

    #[test]
    fn stop_reaches_stopped_from_every_active_state() {
        let dir = TempDir::new("stop").expect("temp dir");
        let mut player = Player::detached();

        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");
        player.stop().expect("stop while playing");
        assert_eq!(player.state(), PlaybackState::Stopped);

        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");
        player.pause().expect("pause");
        player.stop().expect("stop while paused");
        assert_eq!(player.state(), PlaybackState::Stopped);

        // Stopping an idle player is not an error.
        player.stop().expect("stop while stopped");
        assert_eq!(player.state(), PlaybackState::Stopped);
    }

    #[test]
    fn pause_and_resume_while_stopped_are_rejected() {
        let mut player = Player::detached();

        assert!(player.pause().is_err(), "pause without a session must fail");
        assert!(
            player.resume().is_err(),
            "resume without a session must fail"
        );
        assert_eq!(player.state(), PlaybackState::Stopped);
    }

    #[test]
    fn pause_and_resume_after_the_end_are_rejected() {
        let dir = TempDir::new("after-end").expect("temp dir");
        let mut player = Player::detached();
        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");

        let _device = attach_device(&player, 10);
        assert_eq!(player.wait(), PlaybackState::Finished);

        assert!(player.pause().is_err(), "pause after the end must fail");
        assert!(player.resume().is_err(), "resume after the end must fail");
        assert_eq!(player.state(), PlaybackState::Finished);
    }

    #[test]
    fn a_finished_file_can_be_played_again() {
        let dir = TempDir::new("replay").expect("temp dir");
        let first = constant_file(&dir, "first.wav", 0.5);
        let second = constant_file(&dir, "second.wav", -0.5);
        let mut player = Player::detached();

        player.play(&first).expect("play first");
        {
            // The stand-in device drains the session that exists right now.
            let _device = attach_device(&player, 10);
            assert_eq!(player.wait(), PlaybackState::Finished);
            assert!(player.take_error().is_none(), "EOF is not an error");
        }

        // The finished session is replaced: state goes back to Playing and the
        // player keeps working.
        player.play(&second).expect("play second");
        assert_eq!(player.state(), PlaybackState::Playing);
        {
            let _device = attach_device(&player, 10);
            assert_eq!(player.wait(), PlaybackState::Finished);
            assert!(player.take_error().is_none());
        }
    }

    #[test]
    fn pause_freezes_the_position_and_loses_nothing() {
        let dir = TempDir::new("pause-position").expect("temp dir");
        let path = ramp_file(&dir, "ramp.wav");
        let mut player = Player::detached();

        player.play(&path).expect("play");
        // Real time pace, so the file cannot finish before the pause below.
        let device = attach_device(&player, 1);

        assert!(
            wait_until(|| device.played_len() > 4_000),
            "the stand-in device never started playing"
        );

        player.pause().expect("pause");
        assert_eq!(player.state(), PlaybackState::Paused);

        let played_at_pause = device.played_len();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            device.played_len(),
            played_at_pause,
            "a paused player must not consume any buffered sample"
        );

        player.resume().expect("resume");
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.wait(), PlaybackState::Finished);
        assert!(player.take_error().is_none());

        let expected = sample_count(CHANNELS, RATE);
        assert!(
            wait_until(|| device.played_len() == expected),
            "expected {expected} samples to be played, got {}",
            device.played_len()
        );

        // Resuming must continue exactly where the pause happened: a restart
        // would drop back to the beginning of the ramp, a skip would jump ahead.
        let played = device.played();
        assert!(
            played.windows(2).all(|pair| pair[0] <= pair[1]),
            "playback must resume from the paused position, not restart"
        );
        let first = played[0];
        let last = *played.last().expect("something was played");
        assert!(first < -0.85, "first sample {first} should start the ramp");
        assert!(last > 0.85, "last sample {last} should end the ramp");
    }

    #[test]
    fn pausing_after_the_end_of_the_file_still_plays_every_sample() {
        let dir = TempDir::new("pause-drain").expect("temp dir");
        let mut player = Player::detached();
        // The whole file fits in the buffer, so the worker reaches the end of the
        // stream immediately and then waits for the device to play it out.
        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");
        player.pause().expect("pause");

        thread::sleep(Duration::from_millis(100));
        assert_eq!(player.state(), PlaybackState::Paused);

        // Only now does a device show up: it must not be able to play anything
        // while the session is paused, even though the decoder is long done.
        let device = attach_device(&player, 10);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            device.played_len(),
            0,
            "a paused session must not play buffered samples, not even after EOF"
        );
        assert_eq!(player.state(), PlaybackState::Paused);

        player.resume().expect("resume");
        assert_eq!(player.wait(), PlaybackState::Finished);
        assert!(player.take_error().is_none());

        let expected = sample_count(CHANNELS, RATE / 5);
        assert!(
            wait_until(|| device.played_len() == expected),
            "expected {expected} samples after resuming, got {}",
            device.played_len()
        );
    }

    #[test]
    fn play_replaces_the_previous_session_without_leaking_its_samples() {
        let dir = TempDir::new("replace").expect("temp dir");
        // Different values, so any leftover of the first file is obvious.
        let first = constant_file(&dir, "positive.wav", 0.5);
        let second = constant_file(&dir, "negative.wav", -0.5);
        let mut player = Player::detached();

        player.play(&first).expect("play first");
        {
            let device = attach_device(&player, 1);
            assert!(wait_until(|| device.played_len() > 2_000), "nothing played");
            // Dropping the device leaves unplayed samples of the first file in its
            // buffer, which is exactly the situation to guard against.
        }

        // No explicit stop: playing a new file must tear the old session down.
        player.play(&second).expect("play second");
        assert_eq!(player.state(), PlaybackState::Playing);

        let device = attach_device(&player, 10);
        assert_eq!(player.wait(), PlaybackState::Finished);

        let expected = sample_count(CHANNELS, RATE / 5);
        assert!(wait_until(|| device.played_len() == expected));
        let played = device.played();
        assert!(
            played.iter().all(|sample| *sample < 0.0),
            "the new session must not contain samples of the previous file"
        );
    }

    #[test]
    fn a_failed_play_keeps_the_current_session_running() {
        let dir = TempDir::new("failed-play").expect("temp dir");
        let mut player = Player::detached();
        player
            .play(&constant_file(&dir, "good.wav", 0.5))
            .expect("play");

        let error = player
            .play(&dir.join("missing.wav"))
            .expect_err("a missing file must fail");

        assert!(format!("{error:#}").contains("failed to open audio file"));
        assert_eq!(
            player.state(),
            PlaybackState::Playing,
            "a failed play must not interrupt what is already playing"
        );
    }

    #[test]
    fn continuous_operations_do_not_deadlock() {
        let dir = TempDir::new("continuous").expect("temp dir");
        let path = constant_file(&dir, "a.wav", 0.5);
        let mut player = Player::detached();

        player.play(&path).expect("play");
        player.pause().expect("pause");
        player.resume().expect("resume");
        player.pause().expect("pause");
        player.resume().expect("resume");
        player.stop().expect("stop");
        assert_eq!(player.state(), PlaybackState::Stopped);

        player.play(&path).expect("play again");
        player.stop().expect("stop again");
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert!(player.take_error().is_none());
    }

    #[test]
    fn dropping_the_player_stops_the_worker() {
        let dir = TempDir::new("drop").expect("temp dir");
        let mut player = Player::detached();
        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");

        // A live session owns a live decode worker.
        let running = player
            .session
            .as_ref()
            .and_then(|session| session.worker.as_ref())
            .is_some_and(|worker| !worker.is_finished());
        assert!(running, "a session must own a running decode worker");

        // Dropping must wake and join it: if the worker could not be woken this
        // would hang and the test would time out.
        drop(player);
    }

    // ------------------------------------------------------------------- errors

    #[test]
    fn broken_input_files_are_reported_as_errors() {
        let dir = TempDir::new("broken").expect("temp dir");
        let missing = dir.join("missing.wav");

        let empty = dir.join("empty.wav");
        fs::write(&empty, b"").expect("write empty file");

        let invalid = dir.join("invalid.wav");
        fs::write(&invalid, vec![0xAB_u8; 4096]).expect("write invalid file");

        let mut player = Player::detached();

        for (path, expected) in [
            (&missing, "failed to open audio file"),
            (&empty, "unsupported or corrupted audio file"),
            (&invalid, "unsupported or corrupted audio file"),
        ] {
            let error = player.play(path).expect_err("play must fail");
            let message = format!("{error:#}");
            assert!(
                message.contains(expected),
                "unexpected message for {}: {message}",
                path.display()
            );
            assert_eq!(player.state(), PlaybackState::Stopped);
            assert!(
                player.take_error().is_none(),
                "a synchronous failure is returned by play, not reported as a session error"
            );
        }
    }

    #[test]
    fn a_dead_audio_stream_is_reported_and_is_not_a_normal_end() {
        let dir = TempDir::new("stream-error").expect("temp dir");
        let mut player = Player::detached();
        player
            .play(&constant_file(&dir, "a.wav", 0.5))
            .expect("play");

        let buffer = player
            .session
            .as_ref()
            .map(|session| Arc::clone(&session.buffer))
            .expect("session");
        buffer.mark_stream_failed();

        assert_eq!(player.wait(), PlaybackState::Stopped);
        let error = player.take_error().expect("the failure must be reported");
        assert!(format!("{error:#}").contains("stopped unexpectedly"));
        assert_ne!(
            player.state(),
            PlaybackState::Finished,
            "a failure must never be reported as a natural end of stream"
        );
    }

    // --------------------------------------------------- real audio device (opt in)

    /// Writes a sine tone long enough to be audible and reassuringly short.
    fn tone_file(dir: &TempDir, name: &str, seconds: u32) -> std::path::PathBuf {
        let frames = RATE * seconds;
        let path = dir.join(name);
        write_wav(&path, RATE, CHANNELS, frames, |frame| {
            let phase = 2.0 * std::f32::consts::PI * 440.0 * frame as f32 / RATE as f32;
            0.2 * phase.sin()
        })
        .expect("write fixture");
        path
    }

    /// Plays through the real sound card. Ignored by default because it needs an
    /// audio device and makes noise; run it with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires an audio device and plays audible sound"]
    fn real_device_start_pause_resume_stop_and_replay() {
        let dir = TempDir::new("real-device").expect("temp dir");
        let path = tone_file(&dir, "tone.wav", 5);

        let mut player = Player::new();
        player.play(&path).expect("play");
        assert_eq!(player.state(), PlaybackState::Playing);
        println!("output: {}", player.output_info().expect("device info"));

        thread::sleep(Duration::from_millis(300));
        player.pause().expect("pause");
        assert_eq!(player.state(), PlaybackState::Paused);
        thread::sleep(Duration::from_millis(300));

        player.resume().expect("resume");
        assert_eq!(player.state(), PlaybackState::Playing);
        thread::sleep(Duration::from_millis(300));

        player.stop().expect("stop");
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert!(player.take_error().is_none());

        // A stopped player must be reusable.
        player.play(&path).expect("replay");
        assert_eq!(player.state(), PlaybackState::Playing);
    }

    /// Runs a whole file through the real sound card and checks that it reaches
    /// `Finished` on its own. Ignored by default, see above.
    #[test]
    #[ignore = "requires an audio device and plays audible sound"]
    fn real_device_reaches_finished_and_can_play_again() {
        let dir = TempDir::new("real-end").expect("temp dir");
        let first = tone_file(&dir, "first.wav", 1);
        let second = tone_file(&dir, "second.wav", 1);

        let mut player = Player::new();
        player.play(&first).expect("play first");
        assert_eq!(player.wait(), PlaybackState::Finished);
        assert!(player.take_error().is_none(), "EOF is not an error");

        player.play(&second).expect("play second");
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.wait(), PlaybackState::Finished);
        assert!(player.take_error().is_none());
    }
}
