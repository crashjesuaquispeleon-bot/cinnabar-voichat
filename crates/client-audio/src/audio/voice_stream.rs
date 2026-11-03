//! Local microphone stream ownership boundary.
//!
//! Opening a real CPAL input stream is the next platform step. This module defines the
//! handle and conversion helpers so a dedicated owner thread can start/stop capture and
//! push fixed frames into [`super::voice_pcm::BoundedPcmQueue`] without touching Bevy.

use super::voice_pcm::{BoundedPcmQueue, PcmFrame, VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};
use std::sync::Arc;

/// Shared capture queue fed by the owner thread and drained by encode/playback.
pub type SharedPcmQueue = Arc<BoundedPcmQueue>;

/// Errors from opening or running a local capture stream.
#[derive(Debug)]
pub enum VoiceCaptureError {
    /// Policy refused to open the device.
    NotPermitted,
    /// Host reported no input device.
    NoDevice,
    /// Negotiated format cannot be converted to the internal mono layout.
    UnsupportedFormat,
    /// Other platform failure.
    Other(String),
}

impl std::fmt::Display for VoiceCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPermitted => write!(f, "microphone capture is not permitted by policy"),
            Self::NoDevice => write!(f, "no input device available"),
            Self::UnsupportedFormat => write!(f, "unsupported input format"),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for VoiceCaptureError {}

impl From<anyhow::Error> for VoiceCaptureError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value.to_string())
    }
}

/// Lifecycle control for a running (or mock) capture stream.
///
/// Dropping the handle must stop the device and join any owner thread the backend started.
pub trait VoiceCaptureStream: Send {
    /// Stops capture and releases the OS input device.
    fn stop(&mut self);

    /// Whether the underlying device is still open.
    #[must_use]
    fn is_running(&self) -> bool;
}

/// Opens a capture stream that writes into `queue`.
///
/// Implementations must refuse to open when the caller has not already checked
/// [`super::voice_input::VoiceInputPolicy::capture_permitted`]. The stream must stop when
/// the handle is dropped.
pub trait VoiceCaptureOpener: Send + Sync {
    fn open_default(
        &self,
        queue: SharedPcmQueue,
    ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError>;
}

/// Accumulates mono samples into [`PcmFrame`]s and pushes them.
///
/// Intended for the real-time callback path after format conversion.
#[derive(Debug, Default)]
pub struct MonoFrameBuilder {
    partial: [f32; VOICE_FRAME_SAMPLES],
    filled: usize,
}

impl MonoFrameBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends mono `f32` samples, emitting complete frames into `queue`.
    pub fn push_mono_samples(&mut self, samples: &[f32], queue: &BoundedPcmQueue) {
        let mut offset = 0;
        while offset < samples.len() {
            let space = VOICE_FRAME_SAMPLES - self.filled;
            let take = space.min(samples.len() - offset);
            self.partial[self.filled..self.filled + take]
                .copy_from_slice(&samples[offset..offset + take]);
            self.filled += take;
            offset += take;
            if self.filled == VOICE_FRAME_SAMPLES {
                let mut frame = PcmFrame::silence();
                frame.copy_from_slice(&self.partial);
                queue.push(frame);
                self.filled = 0;
            }
        }
    }

    /// Drops any incomplete frame (device stop / session end).
    pub fn reset(&mut self) {
        self.filled = 0;
    }
}

/// Downmixes interleaved samples to mono into caller-owned storage without allocating.
///
/// Returns `None` when `channels` is zero, the input is not channel-aligned, or `dst`
/// is too small. This is the preferred helper for an audio callback.
pub fn interleaved_to_mono_f32_into(
    src: &[f32],
    channels: u16,
    dst: &mut [f32],
) -> Option<usize> {
    if channels == 0 || src.len() % channels as usize != 0 {
        return None;
    }
    let frames = src.len() / channels as usize;
    if dst.len() < frames {
        return None;
    }
    let ch = channels as usize;
    for (frame, output) in src.chunks_exact(ch).zip(dst.iter_mut()) {
        let sum: f32 = frame.iter().copied().sum();
        *output = sum / ch as f32;
    }
    Some(frames)
}

/// Converts integer PCM into caller-owned `f32` storage without allocating.
/// Returns `None` when the destination is too small.
pub fn i16_slice_to_f32_into(src: &[i16], dst: &mut [f32]) -> Option<usize> {
    if dst.len() < src.len() {
        return None;
    }
    for (&sample, output) in src.iter().zip(dst.iter_mut()) {
        *output = f32::from(sample) / 32768.0;
    }
    Some(src.len())
}

/// Downmixes interleaved samples to mono `f32` into `dst` (cleared first).
///
/// Convenience helper for non-real-time callers; it may allocate when capacity is
/// insufficient. `src` length must be a multiple of `channels`.
pub fn interleaved_to_mono_f32(src: &[f32], channels: u16, dst: &mut Vec<f32>) -> usize {
    dst.clear();
    if channels == 0 {
        return 0;
    }
    let ch = channels as usize;
    let frames = src.len() / ch;
    dst.reserve(frames);
    for frame in 0..frames {
        let base = frame * ch;
        let mut sum = 0.0_f32;
        for c in 0..ch {
            sum += src[base + c];
        }
        dst.push(sum / ch as f32);
    }
    frames
}

/// Converts integer PCM (`i16`) samples to `f32` for non-real-time callers; may allocate.
pub fn i16_slice_to_f32(src: &[i16], dst: &mut Vec<f32>) {
    dst.clear();
    dst.reserve(src.len());
    for &s in src {
        dst.push(f32::from(s) / 32768.0);
    }
}

/// Mock stream for tests: records start/stop without touching the OS.
pub struct MockVoiceCaptureStream {
    running: bool,
    queue: SharedPcmQueue,
    builder: MonoFrameBuilder,
}

impl MockVoiceCaptureStream {
    #[must_use]
    pub fn new(queue: SharedPcmQueue) -> Self {
        Self {
            running: true,
            queue,
            builder: MonoFrameBuilder::new(),
        }
    }

    /// Injects mono samples as if a device callback had fired.
    pub fn inject_mono(&mut self, samples: &[f32]) {
        if self.running {
            self.builder.push_mono_samples(samples, &self.queue);
        }
    }
}

impl VoiceCaptureStream for MockVoiceCaptureStream {
    fn stop(&mut self) {
        self.running = false;
        self.builder.reset();
        self.queue.clear();
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for MockVoiceCaptureStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Test opener that always succeeds with a mock stream.
#[derive(Clone, Copy, Debug, Default)]
pub struct MockVoiceCaptureOpener;

impl VoiceCaptureOpener for MockVoiceCaptureOpener {
    fn open_default(
        &self,
        queue: SharedPcmQueue,
    ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
        Ok(Box::new(MockVoiceCaptureStream::new(queue)))
    }
}

/// Target capture rate for documentation and backend negotiation.
#[must_use]
pub fn target_sample_rate() -> u32 {
    VOICE_SAMPLE_RATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::voice_pcm::VOICE_FRAME_SAMPLES;

    #[test]
    fn mono_builder_emits_exact_frames() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        let mut builder = MonoFrameBuilder::new();
        let batch = vec![0.25_f32; VOICE_FRAME_SAMPLES * 2 + 10];
        builder.push_mono_samples(&batch, &queue);
        assert_eq!(queue.len(), 2);
        assert_eq!(builder.filled, 10);
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_none());
    }

    #[test]
    fn interleaved_stereo_downmixes_to_mono() {
        let src = [1.0_f32, 3.0, 5.0, 7.0];
        let mut dst = Vec::new();
        let n = interleaved_to_mono_f32(&src, 2, &mut dst);
        assert_eq!(n, 2);
        assert_eq!(dst, vec![2.0, 6.0]);
    }

    #[test]
    fn mock_stream_stops_on_drop() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        let opener = MockVoiceCaptureOpener;
        let stream = opener.open_default(Arc::clone(&queue)).unwrap();
        assert!(stream.is_running());
        drop(stream);
        let mut stream = opener.open_default(Arc::clone(&queue)).unwrap();
        stream.stop();
        assert!(!stream.is_running());
    }

    #[test]
    fn mock_inject_fills_queue() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        let mut stream = MockVoiceCaptureStream::new(Arc::clone(&queue));
        let samples = vec![0.1_f32; VOICE_FRAME_SAMPLES];
        stream.inject_mono(&samples);
        assert_eq!(queue.len(), 1);
        stream.stop();
        assert!(queue.is_empty());
    }


    #[test]
    fn fixed_buffer_converters_do_not_require_vec_storage() {
        let stereo = [1.0_f32, 3.0, 5.0, 7.0];
        let mut mono = [0.0_f32; 2];
        assert_eq!(interleaved_to_mono_f32_into(&stereo, 2, &mut mono), Some(2));
        assert_eq!(mono, [2.0, 6.0]);
        assert_eq!(interleaved_to_mono_f32_into(&stereo, 2, &mut mono[..1]), None);
        assert_eq!(interleaved_to_mono_f32_into(&stereo[..3], 2, &mut mono), None);

        let integer = [0_i16, 16384, -16384];
        let mut float = [0.0_f32; 3];
        assert_eq!(i16_slice_to_f32_into(&integer, &mut float), Some(3));
        assert!((float[1] - 0.5).abs() < 1e-4);
        assert_eq!(i16_slice_to_f32_into(&integer, &mut float[..2]), None);
    }

    #[test]
    fn mock_stream_preserves_partial_frames_between_callbacks() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        let mut stream = MockVoiceCaptureStream::new(Arc::clone(&queue));
        let first = vec![0.2_f32; VOICE_FRAME_SAMPLES / 2];
        let second = vec![0.4_f32; VOICE_FRAME_SAMPLES / 2];
        stream.inject_mono(&first);
        assert!(queue.is_empty());
        stream.inject_mono(&second);
        assert_eq!(queue.len(), 1);
        let frame = queue.pop().unwrap();
        assert!((frame.samples()[0] - 0.2).abs() < 1e-6);
        assert!((frame.samples()[VOICE_FRAME_SAMPLES / 2] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn i16_conversion_scales() {
        let src = [0_i16, 16384, -16384];
        let mut dst = Vec::new();
        i16_slice_to_f32(&src, &mut dst);
        assert!((dst[0] - 0.0).abs() < 1e-6);
        assert!((dst[1] - 0.5).abs() < 1e-4);
        assert!((dst[2] + 0.5).abs() < 1e-4);
    }
}
