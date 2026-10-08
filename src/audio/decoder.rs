//! Audio decoding.
//!
//! This module owns the `encoded file -> PCM` half of the player: it opens the
//! file, lets Symphonia probe the container, selects the default audio track and
//! then decodes it packet by packet into interleaved `f32` samples.
//!
//! It deliberately knows nothing about audio devices, the CLI, playlists or any
//! future UI.

use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder as SymphoniaDecoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Description of the audio stream that is being decoded.
///
/// The values come from the container header, which is exactly what the audio
/// output has to be opened with. [`Decoder::next_chunk`] verifies that the
/// decoded data really matches them.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Container label derived from the file extension, e.g. `FLAC`.
    pub format: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels; samples are interleaved frame by frame.
    pub channels: usize,
    /// Bits per sample as declared by the container, if it declares any.
    pub bits_per_sample: Option<u32>,
}

/// Streaming decoder that turns an audio file into interleaved `f32` PCM.
pub struct Decoder {
    /// Container reader (demuxer).
    format: Box<dyn FormatReader>,
    /// Codec decoder bound to the selected track.
    decoder: Box<dyn SymphoniaDecoder>,
    /// Track that is played; packets of other tracks are skipped.
    track_id: u32,
    /// Header information, filled in by [`Decoder::open`].
    info: TrackInfo,
    /// Signal specification of the first decoded buffer. Used to detect a
    /// mid-stream format change, which would otherwise produce garbage audio.
    stream_spec: Option<(u32, usize)>,
    /// Interleaved `f32` buffer for the packet that was decoded last.
    sample_buf: Option<SampleBuffer<f32>>,
    /// Set once the end of the stream has been reached.
    finished: bool,
}

impl Decoder {
    /// Opens `path` and prepares it for decoding.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open audio file: {}", path.display()))?;

        // The media source stream adds the buffering and seeking the demuxer needs.
        let source = MediaSourceStream::new(Box::new(file), Default::default());

        // The extension is only a hint: Symphonia still probes the content, so a
        // mislabelled file is detected (or decoded correctly) anyway.
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
            hint.with_extension(extension);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .with_context(|| format!("unsupported or corrupted audio file: {}", path.display()))?;

        let format = probed.format;

        // `track` borrows `format`, so copy out everything we need before moving
        // the reader into `Self`.
        let (track_id, codec_params) = {
            let track = format
                .default_track()
                .context("the file does not contain a playable audio track")?;
            (track.id, track.codec_params.clone())
        };

        let decoder = symphonia::default::get_codecs()
            .make(&codec_params, &DecoderOptions::default())
            .context("failed to create a decoder for the audio track")?;

        let info = TrackInfo {
            format: container_name(path),
            sample_rate: codec_params.sample_rate.unwrap_or(0),
            channels: codec_params.channels.map_or(0, |channels| channels.count()),
            bits_per_sample: codec_params.bits_per_sample,
        };

        if info.sample_rate == 0 || info.channels == 0 {
            bail!(
                "could not determine the stream format of {}: the header declares \
                 {} Hz and {} channel(s)",
                path.display(),
                info.sample_rate,
                info.channels
            );
        }

        Ok(Self {
            format,
            decoder,
            track_id,
            info,
            stream_spec: None,
            sample_buf: None,
            finished: false,
        })
    }

    /// Header information of the file, as used to open the audio output.
    pub fn info(&self) -> &TrackInfo {
        &self.info
    }

    /// Decodes the next packet of the selected track.
    ///
    /// Returns `Ok(None)` at the end of the stream. The returned slice points
    /// into a buffer owned by the decoder and stays valid until the next call,
    /// so callers must copy or consume it before decoding again.
    pub fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        if self.finished {
            return Ok(None);
        }

        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                // Symphonia signals the end of the stream as an unexpected EOF.
                Err(SymphoniaError::IoError(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                    self.finished = true;
                    return Ok(None);
                }
                // The demuxer was re-synchronised (e.g. a chained OGG stream).
                // M1 has no gapless playback, so stop at the track boundary.
                Err(SymphoniaError::ResetRequired) => {
                    self.finished = true;
                    return Ok(None);
                }
                Err(err) => return Err(err).context("failed to read the next audio packet"),
            };

            // Ignore packets belonging to other tracks (cover art, video, ...).
            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                // One broken packet is not worth aborting playback for, but it
                // must never pass silently either.
                Err(SymphoniaError::DecodeError(reason)) => {
                    eprintln!("warning: skipping a corrupt packet: {reason}");
                    continue;
                }
                Err(err) => return Err(err).context("failed to decode an audio packet"),
            };

            // `SampleBuffer` + `copy_interleaved_ref` do the two things the rest
            // of the program relies on, in a single pass:
            //   1. convert the codec's sample format (u8/i16/i24/i32/f32/...) to f32
            //   2. turn planar channel data into interleaved [L, R, L, R, ...]
            let spec = *decoded.spec();
            let channels = spec.channels.count();

            // The audio output was opened for the header's rate/channels, so a
            // stream that disagrees would be played at the wrong speed. Fail
            // loudly instead of producing wrong audio.
            match self.stream_spec {
                None => {
                    if spec.rate != self.info.sample_rate || channels != self.info.channels {
                        bail!(
                            "the decoded stream is {} Hz / {} channel(s) but the header \
                             declared {} Hz / {} channel(s)",
                            spec.rate,
                            channels,
                            self.info.sample_rate,
                            self.info.channels
                        );
                    }
                    self.stream_spec = Some((spec.rate, channels));
                }
                Some(current) if current != (spec.rate, channels) => bail!(
                    "the audio stream changed format during playback \
                     ({} Hz / {} channel(s) instead of {} Hz / {} channel(s))",
                    spec.rate,
                    channels,
                    current.0,
                    current.1
                ),
                Some(_) => {}
            }

            let mut buffer = SampleBuffer::<f32>::new(decoded.frames() as u64, spec);
            buffer.copy_interleaved_ref(decoded);
            self.sample_buf = Some(buffer);

            return Ok(self.sample_buf.as_ref().map(|buffer| buffer.samples()));
        }
    }
}

/// Human readable container label derived from the file extension.
///
/// Symphonia identifies the container from the content but does not expose a
/// printable name for it; the extension is what the user typed and what they
/// expect to see echoed back.
fn container_name(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "unknown".to_owned())
}
