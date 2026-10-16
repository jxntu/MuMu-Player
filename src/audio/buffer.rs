//! The PCM buffer that connects the decode worker to the audio callback.
//!
//! ```text
//! decode worker  --push-->  PcmBuffer  --fill_from_buffer-->  sound card
//! ```
//!
//! The buffer owns three things that must stay consistent with each other, which
//! is why they live behind a single mutex:
//!
//! * the decoded samples that have not been played yet,
//! * the pause flag (a paused callback must not consume samples),
//! * the cancellation / stream-failure flags (they decide when the worker
//!   stops waiting).
//!
//! Keeping the flags in the same critical section as the queue makes the
//! condition-variable predicates race free: a worker that decides to wait can
//! never miss a pause, a resume or a stop.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use cpal::{FromSample, SizedSample};

/// Target buffer length, expressed in seconds of audio.
///
/// Roughly one second is enough to absorb a slow disk read or a scheduler hiccup
/// while keeping memory use small and control commands responsive. It is also the
/// bound on how far the decoder may run ahead of the audio device, which is what
/// makes "pause keeps its position" true and keeps memory usage flat.
const BUFFER_SECONDS: usize = 1;

/// Longest a blocked worker sleeps before re-checking its predicates.
///
/// Waiting threads are normally woken by the condition variable; this timeout is
/// a safety net so that a missed wakeup can slow playback down but never hang it.
pub const WORKER_WAIT_TIMEOUT: Duration = Duration::from_millis(20);

/// The part of the buffer protected by its mutex.
#[derive(Debug)]
struct Shared {
    /// Interleaved `f32` samples not handed to the device yet.
    samples: VecDeque<f32>,
    /// Set once the producer will not push any more samples.
    producer_finished: bool,
    /// Set by the callback when it ran out of samples after the producer
    /// finished, i.e. everything that was queued has reached the device.
    drained: bool,
    /// While set, the callback outputs silence and leaves `samples` untouched.
    paused: bool,
    /// While set, the decode worker leaves its loops as soon as it can.
    cancelled: bool,
    /// Set by the CPAL error callback if the stream dies.
    stream_failed: bool,
}

/// PCM buffer shared by one playback session.
///
/// A session always gets a fresh buffer, which is what guarantees that no sample
/// of an old session can ever be played by a new one.
#[derive(Debug)]
pub struct PcmBuffer {
    shared: Mutex<Shared>,
    /// Signalled whenever `shared` changes in a way a waiting thread cares about.
    changed: Condvar,
    /// Soft upper bound for `samples.len()`, in samples.
    capacity: usize,
}

impl PcmBuffer {
    /// Creates an empty buffer for a stream with these parameters.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let capacity = sample_rate as usize * channels as usize * BUFFER_SECONDS;
        Self {
            shared: Mutex::new(Shared {
                samples: VecDeque::with_capacity(capacity),
                producer_finished: false,
                drained: false,
                paused: false,
                cancelled: false,
                stream_failed: false,
            }),
            changed: Condvar::new(),
            capacity,
        }
    }

    /// Samples that can still be queued before the producer should wait.
    pub fn free_space(&self) -> usize {
        self.capacity.saturating_sub(self.lock().samples.len())
    }

    /// Queues interleaved samples. Only the decode worker calls this.
    ///
    /// The buffer may overshoot [`Self::free_space`]'s limit by one chunk instead
    /// of dropping samples, which keeps this method infallible.
    pub fn push(&self, samples: &[f32]) {
        self.lock().samples.extend(samples);
    }

    /// Tells the callback that no more samples will arrive.
    pub fn finish_input(&self) {
        self.lock().producer_finished = true;
    }

    /// `true` once every queued sample has been handed to the device.
    pub fn is_drained(&self) -> bool {
        let shared = self.lock();
        shared.producer_finished && shared.drained
    }

    /// Asks the audio callback to output silence without consuming samples.
    ///
    /// This is what makes a pause a pause: the samples stay in the buffer, so the
    /// playback position cannot move and nothing that should still be played is
    /// lost.
    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
        self.changed.notify_all();
    }

    /// `true` while playback is suspended.
    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    /// Asks the decode worker to leave its loops as soon as possible.
    pub fn cancel(&self) {
        self.lock().cancelled = true;
        self.changed.notify_all();
    }

    /// `true` once [`Self::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Records that the audio stream died. Called from the CPAL error callback.
    pub fn mark_stream_failed(&self) {
        self.lock().stream_failed = true;
        self.changed.notify_all();
    }

    /// `true` if the audio callback reported a fatal stream error.
    pub fn stream_failed(&self) -> bool {
        self.lock().stream_failed
    }

    /// Blocks until the buffer has room for more samples.
    ///
    /// Returns early when the session is cancelled or the stream failed, so a
    /// paused or stopped session can never leave the worker waiting forever.
    pub fn wait_for_space(&self) {
        let capacity = self.capacity;
        self.wait_until(|shared| {
            shared.cancelled || shared.stream_failed || shared.samples.len() < capacity
        });
    }

    /// Blocks until everything queued has been played.
    ///
    /// Returns early when the session is cancelled or the stream failed.
    pub fn wait_until_drained(&self) {
        self.wait_until(|shared| {
            shared.cancelled || shared.stream_failed || (shared.producer_finished && shared.drained)
        });
    }

    /// Consumes up to `out.len()` samples, filling the rest with silence.
    ///
    /// This is the body of the realtime callback, kept here (and free of device
    /// specific types) so the tests can drive exactly the same code with their
    /// own clock instead of a sound card.
    ///
    /// Returns how many samples were taken from the buffer.
    pub(crate) fn fill_from_buffer<T>(&self, out: &mut [T]) -> usize
    where
        T: SizedSample + FromSample<f32>,
    {
        let mut shared = self.lock();

        if shared.paused {
            // Paused: hand the device silence and leave the buffer untouched. The
            // position therefore cannot move and no sample can be lost.
            for dst in out.iter_mut() {
                *dst = T::from_sample(0.0f32);
            }
            return 0;
        }

        // Whole frames only: the worker pushes whole frames and the device asks
        // for whole frames, so channels can never be swapped between callbacks.
        let available = shared.samples.len().min(out.len());
        for (dst, sample) in out.iter_mut().zip(shared.samples.drain(..available)) {
            *dst = T::from_sample(sample);
        }

        if available > 0 {
            // Space appeared: wake a worker that is waiting for back-pressure.
            self.changed.notify_all();
        }

        if available < out.len() {
            // Underrun. CPAL pre-fills the buffer with silence, but writing it
            // explicitly guarantees that stale data is never played.
            for dst in &mut out[available..] {
                *dst = T::from_sample(0.0f32);
            }

            if shared.producer_finished {
                // The producer is done *and* the buffer is empty: everything that
                // was decoded has now reached the device.
                shared.drained = true;
                self.changed.notify_all();
            }
        }

        available
    }

    /// Waits on the condition variable until `ready` holds.
    ///
    /// The predicate is re-evaluated on every wakeup *and* after every timeout, so
    /// losing a notification only costs [`WORKER_WAIT_TIMEOUT`] of latency.
    fn wait_until(&self, ready: impl Fn(&Shared) -> bool) {
        let mut shared = self.lock();
        while !ready(&shared) {
            shared = match self.changed.wait_timeout(shared, WORKER_WAIT_TIMEOUT) {
                Ok((shared, _timeout)) => shared,
                // A poisoned mutex would mean a panic elsewhere; the data itself
                // is still consistent, so keep going rather than give up.
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Locks the shared state, recovering from poisoning.
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
