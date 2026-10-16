//! Audio output: the CPAL stream and the PCM buffer that feeds it.
//!
//! The decode worker (an ordinary thread) produces interleaved `f32` samples,
//! the CPAL callback (a realtime thread) consumes them:
//!
//! ```text
//! decode worker  --push-->  PcmBuffer  --callback-->  sound card
//! ```
//!
//! The buffer decouples the two, so the realtime callback never performs file IO,
//! decoding or anything else that could miss its deadline. All pause / cancel /
//! drain semantics live in `PcmBuffer`, which this module only wires to a device.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

use super::buffer::PcmBuffer;

/// What the audio stream was actually opened with.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// Name of the device the stream was opened on.
    pub device_name: String,
    /// Sample rate of the stream, in Hz.
    pub sample_rate: u32,
    /// Number of channels of the stream.
    pub channels: u16,
    /// Sample format the callback writes to the device.
    pub sample_format: SampleFormat,
}

impl fmt::Display for OutputInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({} Hz, {} channel(s), {})",
            self.device_name, self.sample_rate, self.channels, self.sample_format
        )
    }
}

/// Owns the CPAL output stream of one playback session.
///
/// The stream is created per session and dropped when the session ends, which
/// both stops the callback and releases the device.
pub struct AudioOutput {
    stream: Stream,
    buffer: Arc<PcmBuffer>,
    info: OutputInfo,
}

impl AudioOutput {
    /// Opens the system default output device for `channels` channels at
    /// `sample_rate` Hz.
    ///
    /// No resampling is performed: the device has to support the file's rate and
    /// channel count directly. If it does not, a clear error is returned instead
    /// of playing the file at the wrong pitch.
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self> {
        if sample_rate == 0 {
            bail!("cannot open the audio output: the file declares no sample rate");
        }
        if channels == 0 {
            bail!("cannot open the audio output: the file declares no audio channels");
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default audio output device is available")?;

        let supported = select_output_config(&device, sample_rate, channels)?;
        let sample_format = supported.sample_format();
        let config = supported.config();

        let buffer = Arc::new(PcmBuffer::new(sample_rate, channels));
        let stream = build_stream(&device, config, sample_format, Arc::clone(&buffer))?;

        Ok(Self {
            stream,
            buffer,
            info: OutputInfo {
                device_name: device.to_string(),
                sample_rate,
                channels,
                sample_format,
            },
        })
    }

    /// Starts the stream. Until this is called the callback is never invoked.
    pub fn start(&self) -> Result<()> {
        self.stream.play().context("failed to start audio playback")
    }

    /// The PCM buffer feeding this stream.
    pub fn buffer(&self) -> &Arc<PcmBuffer> {
        &self.buffer
    }

    /// How the stream was actually opened.
    pub fn info(&self) -> &OutputInfo {
        &self.info
    }
}

/// Picks an output configuration that matches `sample_rate` and `channels`.
fn select_output_config(
    device: &Device,
    sample_rate: u32,
    channels: u16,
) -> Result<cpal::SupportedStreamConfig> {
    let ranges = device
        .supported_output_configs()
        .context("failed to query the capabilities of the audio output device")?;

    let mut best: Option<cpal::SupportedStreamConfigRange> = None;
    for range in ranges {
        if range.channels() != channels || !range.contains_rate(sample_rate) {
            continue;
        }
        let is_better = best.is_none_or(|current| {
            sample_format_rank(range.sample_format()) < sample_format_rank(current.sample_format())
        });
        if is_better {
            best = Some(range);
        }
    }

    match best {
        Some(range) => Ok(range.with_sample_rate(sample_rate)),
        None => {
            // Point the user at the device default: that is almost always the
            // rate they would have to convert to (which M1 does not do yet).
            let hint = match device.default_output_config() {
                Ok(default) => format!(
                    "; the device default is {} Hz / {} channel(s) / {}",
                    default.sample_rate(),
                    default.channels(),
                    default.sample_format()
                ),
                Err(_) => String::new(),
            };
            bail!(
                "the audio output device does not support {sample_rate} Hz with \
                 {channels} channel(s){hint}"
            )
        }
    }
}

/// Preference order when several configurations match.
///
/// Float first (the player's native format, so no conversion at all), then the
/// integer formats that are actually used by Linux audio devices. Formats ranked
/// `u8::MAX` are not converted by [`build_stream`].
fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I16 => 2,
        SampleFormat::F64 => 3,
        SampleFormat::U16 => 4,
        SampleFormat::I8 => 5,
        SampleFormat::U8 => 6,
        _ => u8::MAX,
    }
}

/// Builds the output stream with the sample type the device asked for.
///
/// The arms only differ in `T`; the conversion from the player's `f32` samples
/// happens inside the callback through `FromSample`.
fn build_stream(
    device: &Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    buffer: Arc<PcmBuffer>,
) -> Result<Stream> {
    macro_rules! build_typed {
        ($sample:ty) => {
            build_typed_stream::<$sample>(device, config, buffer)
        };
    }

    match sample_format {
        SampleFormat::F32 => build_typed!(f32),
        SampleFormat::I32 => build_typed!(i32),
        SampleFormat::I16 => build_typed!(i16),
        SampleFormat::F64 => build_typed!(f64),
        SampleFormat::U16 => build_typed!(u16),
        SampleFormat::I8 => build_typed!(i8),
        SampleFormat::U8 => build_typed!(u8),
        other => Err(anyhow!("unsupported output sample format: {other}")),
    }
}

/// Creates the stream for one concrete sample type.
fn build_typed_stream<T>(
    device: &Device,
    config: StreamConfig,
    buffer: Arc<PcmBuffer>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let error_buffer = Arc::clone(&buffer);
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _info| {
                // The callback body lives in `PcmBuffer`: it is pure buffer
                // bookkeeping and is therefore shared verbatim with the stand-in
                // device the test suite drives.
                buffer.fill_from_buffer(out);
            },
            move |err| {
                // Runs on the audio thread: keep it to reporting only. The flag
                // is what lets the player notice a dead stream.
                eprintln!("Error: audio output stream failed: {err}");
                error_buffer.mark_stream_failed();
            },
            None,
        )
        .context("failed to create the audio output stream")?;

    Ok(stream)
}
