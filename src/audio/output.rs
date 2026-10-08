//! Audio output.
//!
//! Owns the CPAL output stream and the sample queue that feeds it. The decoder
//! (normal thread) produces interleaved `f32` samples, the CPAL callback
//! (realtime thread) consumes them:
//!
//! ```text
//! decoder / main thread  --push-->  queue  --callback-->  sound card
//! ```
//!
//! The queue decouples the two, so the realtime callback never performs file IO
//! or decoding.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

/// Target queue length expressed in seconds of audio.
///
/// Roughly one second is enough to absorb a slow disk read or a scheduler hiccup
/// while still keeping the memory use small and playback responsive.
const QUEUE_SECONDS: usize = 1;

/// Queue shared between the decoding thread and the realtime audio callback.
#[derive(Debug)]
struct Queue {
    /// Interleaved `f32` samples not yet handed to the device.
    samples: VecDeque<f32>,
    /// Set once the decoder will not push any more samples.
    producer_finished: bool,
    /// Set by the callback when it ran out of real samples after the producer
    /// finished, i.e. everything queued has been handed to the device.
    drained: bool,
}

/// Owns the CPAL output stream and the PCM queue feeding it.
pub struct AudioOutput {
    stream: Stream,
    queue: Arc<Mutex<Queue>>,
    /// Soft upper bound for [`Queue::samples`], used for back-pressure only.
    capacity: usize,
    device_name: String,
    config: StreamConfig,
    sample_format: SampleFormat,
    /// Set by the error callback if the stream dies (unplugged device, ...).
    failed: Arc<AtomicBool>,
}

impl AudioOutput {
    /// Opens the system default output device for `channels` channels at
    /// `sample_rate` Hz.
    ///
    /// No resampling is performed: the device has to support the file's rate and
    /// channel count directly. If it does not, a clear error is returned instead
    /// of playing the file at the wrong pitch.
    pub fn new(sample_rate: u32, channels: usize) -> Result<Self> {
        if sample_rate == 0 {
            bail!("cannot open the audio output: the file declares no sample rate");
        }
        let channels = u16::try_from(channels)
            .context("cannot open the audio output: the file has too many channels")?;
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

        let capacity = sample_rate as usize * channels as usize * QUEUE_SECONDS;
        let queue = Arc::new(Mutex::new(Queue {
            samples: VecDeque::with_capacity(capacity),
            producer_finished: false,
            drained: false,
        }));
        let failed = Arc::new(AtomicBool::new(false));

        let stream = build_stream(
            &device,
            config,
            sample_format,
            Arc::clone(&queue),
            Arc::clone(&failed),
        )?;

        Ok(Self {
            stream,
            queue,
            capacity,
            device_name: device.to_string(),
            config,
            sample_format,
            failed,
        })
    }

    /// Starts (or resumes) playback of the stream.
    pub fn start(&self) -> Result<()> {
        self.stream.play().context("failed to start audio playback")
    }

    /// Number of samples that can still be queued before the producer should
    /// wait. Zero means "the queue is full, give the callback time to catch up".
    pub fn free_space(&self) -> usize {
        self.capacity.saturating_sub(self.lock().samples.len())
    }

    /// Queues interleaved samples. Only called from the decoding thread.
    ///
    /// The queue is allowed to overshoot [`Self::capacity`] by one chunk rather
    /// than dropping samples, which keeps this method infallible.
    pub fn push(&self, samples: &[f32]) {
        self.lock().samples.extend(samples);
    }

    /// Tells the callback that the producer is done, so playback can end once
    /// the queue has been played out.
    pub fn finish_input(&self) {
        self.lock().producer_finished = true;
    }

    /// `true` once every queued sample has been handed to the device.
    pub fn is_drained(&self) -> bool {
        let queue = self.lock();
        queue.producer_finished && queue.drained
    }

    /// `true` if the audio callback reported a fatal stream error.
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Name of the device the stream was opened on.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Sample rate of the opened stream, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    /// Number of channels of the opened stream.
    pub fn channels(&self) -> u16 {
        self.config.channels
    }

    /// Sample format of the opened stream.
    pub fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }

    /// Locks the queue.
    ///
    /// A poisoned mutex would mean a panic in the audio callback; recovering the
    /// (still consistent) data is better than taking the whole process down.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
/// The arms only differ in `T`; the conversion from the decoder's `f32` samples
/// happens inside the callback through `FromSample`.
fn build_stream(
    device: &Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    queue: Arc<Mutex<Queue>>,
    failed: Arc<AtomicBool>,
) -> Result<Stream> {
    macro_rules! build_typed {
        ($sample:ty) => {
            build_typed_stream::<$sample>(device, config, queue, failed)
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
    queue: Arc<Mutex<Queue>>,
    failed: Arc<AtomicBool>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _info| fill_from_queue(out, &queue),
            move |err| {
                // Runs on the audio thread: keep it to reporting only.
                eprintln!("Error: audio output stream failed: {err}");
                failed.store(true, Ordering::Relaxed);
            },
            None,
        )
        .context("failed to create the audio output stream")?;

    Ok(stream)
}

/// Realtime callback: copies queued samples into the device buffer.
///
/// Must stay allocation free and short. The producer never blocks while holding
/// the lock (it waits *outside* the critical section), so the lock here is only
/// ever held for a short memcpy.
fn fill_from_queue<T>(out: &mut [T], queue: &Arc<Mutex<Queue>>)
where
    T: SizedSample + FromSample<f32>,
{
    let mut queue = match queue.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Whole frames only: the decoder pushes whole frames and the device asks for
    // whole frames, so channels can never be swapped between two callbacks.
    let available = queue.samples.len().min(out.len());
    for (dst, sample) in out.iter_mut().zip(queue.samples.drain(..available)) {
        *dst = T::from_sample(sample);
    }

    if available < out.len() {
        // Underrun. CPAL pre-fills the buffer with silence, but writing it
        // explicitly guarantees that stale data is never played.
        for dst in &mut out[available..] {
            *dst = T::from_sample(0.0f32);
        }

        if queue.producer_finished {
            queue.drained = true;
        }
    }
}
