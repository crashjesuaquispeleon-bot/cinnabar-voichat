//! Pre-allocated bounded PCM queue for local voice capture.
//!
//! Storage is reserved when the queue is created. The capture callback path must only
//! copy samples into an already-owned slot and never allocate, log, encode or block.

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
}

impl PcmFrame {
    #[must_use]
    pub const fn silence() -> Self {
        Self {
            samples: [0.0; VOICE_FRAME_SAMPLES],
        }
    }

    /// Copies as many samples as fit. Remaining slots stay unchanged.
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
}

impl Default for PcmFrame {
    fn default() -> Self {
        Self::silence()
    }
}

/// Fixed-capacity ring of PCM frames. Drop-oldest on overflow.
///
/// Construct once when capture starts; reuse for the lifetime of the stream.
#[derive(Debug)]
pub struct BoundedPcmQueue {
    slots: Box<[PcmFrame]>,
    head: usize,
    len: usize,
    pushed: u64,
    dropped: u64,
}

impl BoundedPcmQueue {
    /// Allocates `capacity` silent frames up front.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            slots: vec![PcmFrame::silence(); capacity].into_boxed_slice(),
            head: 0,
            len: 0,
            pushed: 0,
            dropped: 0,
        }
    }

    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(VOICE_QUEUE_CAPACITY)
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Pushes a frame. If full, drops the oldest frame and counts it.
    pub fn push(&mut self, frame: PcmFrame) {
        let capacity = self.slots.len();
        if self.len == capacity {
            self.head = (self.head + 1) % capacity;
            self.len -= 1;
            self.dropped += 1;
        }
        let index = (self.head + self.len) % capacity;
        self.slots[index] = frame;
        self.len += 1;
        self.pushed += 1;
    }

    /// Copies `src` into the next free slot (or over the oldest if full).
    /// Intended for the capture callback: no allocation.
    pub fn push_samples(&mut self, src: &[f32]) {
        let capacity = self.slots.len();
        if self.len == capacity {
            self.head = (self.head + 1) % capacity;
            self.len -= 1;
            self.dropped += 1;
        }
        let index = (self.head + self.len) % capacity;
        self.slots[index].copy_from_slice(src);
        self.len += 1;
        self.pushed += 1;
    }

    #[must_use]
    pub fn pop(&mut self) -> Option<PcmFrame> {
        if self.len == 0 {
            return None;
        }
        let frame = self.slots[self.head];
        self.head = (self.head + 1) % self.slots.len();
        self.len -= 1;
        Some(frame)
    }

    /// Drops every queued frame (session end, mute device stop, etc.).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
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
        let mut q = BoundedPcmQueue::new(3);
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
    fn overflow_drops_oldest_and_counts() {
        let mut q = BoundedPcmQueue::new(2);
        for i in 0..5 {
            let mut frame = PcmFrame::silence();
            frame.samples[0] = i as f32;
            q.push(frame);
        }
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 3);
        assert_eq!(q.pop().unwrap().samples()[0], 3.0);
        assert_eq!(q.pop().unwrap().samples()[0], 4.0);
    }

    #[test]
    fn push_samples_does_not_allocate_new_slots() {
        let mut q = BoundedPcmQueue::with_default_capacity();
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
    fn clear_empties_without_resetting_counters() {
        let mut q = BoundedPcmQueue::new(2);
        q.push(PcmFrame::silence());
        q.push(PcmFrame::silence());
        q.push(PcmFrame::silence());
        assert_eq!(q.dropped(), 1);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.pushed(), 3);
    }
}
