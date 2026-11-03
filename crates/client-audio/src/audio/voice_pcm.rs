//! Pre-allocated bounded PCM queue for local voice capture.
//!
//! Storage is reserved when the queue is created. The capture callback path must only
//! copy samples into an already-owned slot and never allocate, log, encode or block.
//! This queue is safe to share between the capture callback and a consumer thread.

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};

/// Internal capture format used between the microphone backend and later encode/playback.
pub const VOICE_SAMPLE_RATE: u32 = 48_000;
/// Mono keeps bandwidth and mixing cost low for speech.
pub const VOICE_CHANNELS: u16 = 1;
/// 20 ms frames align with common Opus frame sizes.
pub const VOICE_FRAME_DURATION_MS: u32 = 20;
/// Samples per mono frame at [`VOICE_SAMPLE_RATE`].
pub const VOICE_FRAME_SAMPLES: usize =
    (VOICE_SAMPLE_RATE as usize * VOICE_FRAME_DURATION_MS as usize) / 1000;
/// Default depth (~300 ms) absorbs brief scheduling hiccups without high latency.
pub const VOICE_QUEUE_CAPACITY: usize = 15;

/// One fixed-size mono PCM frame.
#[derive(Clone, Copy, Debug)]
pub struct PcmFrame {
    samples: [f32; VOICE_FRAME_SAMPLES],
    generation: u64,
}

impl PcmFrame {
    #[must_use]
    pub const fn silence() -> Self {
        Self {
            samples: [0.0; VOICE_FRAME_SAMPLES],
            generation: 0,
        }
    }

    /// Copies as many samples as fit. Remaining slots are zero-filled.
    pub fn copy_from_slice(&mut self, src: &[f32]) {
        let n = src.len().min(VOICE_FRAME_SAMPLES);
        self.samples[..n].copy_from_slice(&src[..n]);
        if n < VOICE_FRAME_SAMPLES {
            self.samples[n..].fill(0.0);
        }
    }

    #[must_use]
    pub fn samples(&self) -> &[f32; VOICE_FRAME_SAMPLES] {
        &self.samples
    }

    #[must_use]
    pub fn is_current_for(&self, queue: &BoundedPcmQueue) -> bool {
        queue.accepting.load(Ordering::Acquire)
            && self.generation == queue.generation.load(Ordering::Acquire)
    }
}

impl Default for PcmFrame {
    fn default() -> Self {
        Self::silence()
    }
}

/// Fixed-capacity, lock-free queue of PCM frames.
///
/// The producer never waits. If full, the newest frame is rejected and counted; retaining
/// queued audio is preferable to having the real-time callback mutate a consumer-owned
/// ring head. Construct once before starting capture.
#[derive(Debug)]
pub struct BoundedPcmQueue {
    slots: ArrayQueue<PcmFrame>,
    pushed: AtomicU64,
    dropped: AtomicU64,
    suppressed: AtomicU64,
    accepting: std::sync::atomic::AtomicBool,
    generation: AtomicU64,
}

impl BoundedPcmQueue {
    /// Allocates `capacity` frame slots up front.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: ArrayQueue::new(capacity.max(1)),
            pushed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            accepting: std::sync::atomic::AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(VOICE_QUEUE_CAPACITY)
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether microphone frames may enter the queue under the current policy.
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    /// Changes the capture gate and advances an epoch so in-flight callbacks from an
    /// earlier PTT/mute/session state can never be consumed after the gate reopens.
    pub fn set_accepting(&self, accepting: bool) {
        let current = self.accepting.load(Ordering::Acquire);
        if current == accepting {
            return;
        }
        if !accepting {
            self.accepting.store(false, Ordering::Release);
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.clear();
        } else {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.accepting.store(true, Ordering::Release);
        }
    }

    #[must_use]
    pub fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn pushed(&self) -> u64 {
        self.pushed.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Enqueues a frame without waiting. A full queue drops the newest frame.
    pub fn push(&self, mut frame: PcmFrame) {
        if !self.accepting.load(Ordering::Acquire) {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        if !self.accepting.load(Ordering::Acquire) {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        frame.generation = generation;
        if self.slots.push(frame).is_ok() {
            self.pushed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Copies `src` into a fixed-size stack frame, then enqueues it without allocation.
    pub fn push_samples(&self, src: &[f32]) {
        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(src);
        self.push(frame);
    }

    #[must_use]
    pub fn pop(&self) -> Option<PcmFrame> {
        if !self.accepting.load(Ordering::Acquire) {
            return None;
        }
        let generation = self.generation.load(Ordering::Acquire);
        loop {
            let frame = self.slots.pop()?;
            if frame.generation == generation
                && self.accepting.load(Ordering::Acquire)
                && self.generation.load(Ordering::Acquire) == generation
            {
                return Some(frame);
            }
        }
    }

    /// Drops every queued frame (session end, mute, device stop, etc.).
    pub fn clear(&self) {
        while self.slots.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_constants_match_twenty_millisecond_mono_48k() {
        assert_eq!(VOICE_FRAME_SAMPLES, 960);
        assert_eq!(VOICE_CHANNELS, 1);
    }

    #[test]
    fn queue_preserves_order_until_capacity() {
        let q = BoundedPcmQueue::new(3);
        q.set_accepting(true);
        for i in 0..3 {
            let mut frame = PcmFrame::silence();
            frame.samples[0] = i as f32;
            q.push(frame);
        }
        assert_eq!(q.len(), 3);
        assert_eq!(q.dropped(), 0);
        assert_eq!(q.pop().unwrap().samples()[0], 0.0);
        assert_eq!(q.pop().unwrap().samples()[0], 1.0);
        assert_eq!(q.pop().unwrap().samples()[0], 2.0);
        assert!(q.pop().is_none());
    }

    #[test]
    fn overflow_drops_newest_and_counts() {
        let q = BoundedPcmQueue::new(2);
        q.set_accepting(true);
        for i in 0..5 {
            let mut frame = PcmFrame::silence();
            frame.samples[0] = i as f32;
            q.push(frame);
        }
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 3);
        assert_eq!(q.pop().unwrap().samples()[0], 0.0);
        assert_eq!(q.pop().unwrap().samples()[0], 1.0);
    }

    #[test]
    fn push_samples_keeps_capacity_stable() {
        let q = BoundedPcmQueue::with_default_capacity();
        q.set_accepting(true);
        let capacity = q.capacity();
        let src = [0.5_f32; VOICE_FRAME_SAMPLES];
        for _ in 0..(capacity + 4) {
            q.push_samples(&src);
        }
        assert_eq!(q.capacity(), capacity);
        assert_eq!(q.len(), capacity);
        assert_eq!(q.dropped(), 4);
    }

    #[test]
    fn closed_gate_rejects_samples_and_old_epoch_frames() {
        let q = BoundedPcmQueue::new(2);
        assert!(!q.is_accepting());
        q.push(PcmFrame::silence());
        assert_eq!(q.suppressed(), 1);
        assert!(q.is_empty());

        q.set_accepting(true);
        let mut in_flight = PcmFrame::silence();
        in_flight.generation = q.generation.load(Ordering::Acquire);
        q.set_accepting(false);
        q.set_accepting(true);
        // Simulate a callback that read the old epoch before disable, then enqueued late.
        q.slots.push(in_flight).unwrap();
        assert!(q.pop().is_none());
    }

    #[test]
    fn clear_empties_without_resetting_counters() {
        let q = BoundedPcmQueue::new(2);
        q.set_accepting(true);
        q.push(PcmFrame::silence());
        q.push(PcmFrame::silence());
        q.push(PcmFrame::silence());
        assert_eq!(q.dropped(), 1);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.pushed(), 2);
    }
}
