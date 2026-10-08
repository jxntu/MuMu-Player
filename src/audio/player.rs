//! Playback orchestration.
//!
//! Glues the decoder and the audio output together:
//!
//! ```text
//! Decoder --&[f32]--> AudioOutput (queue) --> CPAL --> sound card
//! ```
//!
//! The decoder runs on the calling thread, the samples are handed to the audio
//! callback through the queue in [`AudioOutput`].

use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::decoder::Decoder;
use super::output::AudioOutput;

/// How long the decoding thread waits before checking again whether the audio
/// callback has freed up space in the queue.
const BACKPRESSURE_SLEEP: Duration = Duration::from_millis(5);

/// Extra time granted to the device after the queue drained, so the last buffer
/// handed over is actually audible before the stream is dropped.
const DRAIN_TAIL: Duration = Duration::from_millis(200);

/// Coordinates decoding and audio output.
#[derive(Default)]
pub struct Player {
    /// Output stream of the file currently being played.
    ///
    /// Held as a field rather than a local so a later milestone can pause,
    /// resume or seek without rebuilding the stream.
    output: Option<AudioOutput>,
}

impl Player {
    /// Creates a player without an active output stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes `path` and plays it through the default audio device, blocking
    /// until the end of the stream.
    pub fn play(&mut self, path: &Path) -> Result<()> {
        let mut decoder = Decoder::open(path)?;
        // Owned copy: `info` borrows the decoder, which is borrowed mutably in
        // the playback loop below.
        let info = decoder.info().clone();

        println!("Playing: {}", display_name(path));
        println!("Format: {}", info.format);
        println!("Sample rate: {} Hz", info.sample_rate);
        println!("Channels: {}", info.channels);
        if let Some(bits) = info.bits_per_sample {
            println!("Bit depth: {bits} bit");
        }

        self.output = Some(AudioOutput::new(info.sample_rate, info.channels)?);
        let output = match self.output.as_ref() {
            Some(output) => output,
            None => return Err(anyhow!("audio output was not initialised")),
        };

        println!(
            "Output device: {} ({} Hz, {} channel(s), {})",
            output.device_name(),
            output.sample_rate(),
            output.channels(),
            output.sample_format()
        );
        println!("Press Ctrl+C to stop.");

        output.start()?;

        // Producer loop: decode, queue, repeat. The queue is bounded, so a long
        // file never has to be held in memory.
        loop {
            if output.has_failed() {
                return Err(anyhow!("the audio output stream stopped unexpectedly"));
            }

            if output.free_space() == 0 {
                // Wait outside the lock: the audio callback must always be able
                // to grab the queue immediately.
                thread::sleep(BACKPRESSURE_SLEEP);
                continue;
            }

            match decoder.next_chunk()? {
                Some(samples) => output.push(samples),
                None => break,
            }
        }

        // Everything is decoded; let the callback play out what is still queued.
        output.finish_input();
        while !output.is_drained() {
            if output.has_failed() {
                return Err(anyhow!("the audio output stream stopped unexpectedly"));
            }
            thread::sleep(BACKPRESSURE_SLEEP);
        }

        thread::sleep(DRAIN_TAIL);
        println!("Playback finished.");
        Ok(())
    }
}

/// File name of `path`, or the full display path if it does not have one.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
