//! Helpers for the test suite: generate small PCM WAV files on the fly.
//!
//! The tests must not depend on files that happen to exist on the machine running
//! them, so every fixture is written here. Keeping the writer in-tree avoids
//! pulling in an audio library (or a temp-file crate) just for tests.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A directory that is removed again when it goes out of scope.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a uniquely named directory below the system temp directory.
    pub fn new(label: &str) -> io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "linux-player-test-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Full path of `name` inside the directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover temp directory must never fail a test.
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes a 16-bit PCM WAV file whose sample values come from `value(frame)`.
///
/// All channels of a frame get the same value, so a test can reason about the
/// decoded stream as a plain sequence.
pub fn write_wav(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    frames: u32,
    value: impl Fn(u32) -> f32,
) -> io::Result<()> {
    let block_align = channels * 2;
    let data_len = frames * u32::from(block_align);
    let mut out = Vec::with_capacity(44 + data_len as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    // fmt chunk: uncompressed 16 bit PCM.
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk.
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let sample = to_i16(value(frame));
        for _ in 0..channels {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }

    let mut file = File::create(path)?;
    file.write_all(&out)?;
    file.flush()
}

/// Number of samples a WAV written by [`write_wav`] decodes to.
pub fn sample_count(channels: u16, frames: u32) -> usize {
    frames as usize * usize::from(channels)
}

/// Converts a `-1.0 ..= 1.0` sample into the 16-bit domain.
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}
