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
#[derive(Debug, thiserror::Error)]
pub enum VoiceCaptureError {
    #[error("microphone capture is not permitted by policy")]
    NotPermitted,
    #[error("no input device available")]
    NoDevice,
    #[error("unsupported input format")]
    UnsupportedFormat,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
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
    fn open_default(&self, queue: SharedPcmQueue) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError>;
}

/// Accumulates interleaved or planar samples into mono [`PcmFrame`]s and pushes them.
///
/// Intended for the real-time callback: no heap allocation after construction of the
/// scratch frame on the stack inside [`push_mono_samples`].
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

/// Downmixes interleaved samples to mono `f32` into `dst` (cleared first).
///
/// `src` length must be a multiple of `channels`. Returns the number of mono samples written.
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

/// Converts integer PCM (`i16`) samples to `f32` in place-style output buffer.
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
}

impl MockVoiceCaptureStream {
    #[must_use]
    pub fn new(queue: SharedPcmQueue) -> Self {
        Self {
            running: true,
            queue,
        }
    }

    /// Injects mono samples as if a device callback had fired.
    pub fn inject_mono(&mut self, samples: &[f32]) {
        if self.running {
            let mut builder = MonoFrameBuilder::new();
            builder.push_mono_samples(samples, &self.queue);
        }
    }
}

impl VoiceCaptureStream for MockVoiceCaptureStream {
    fn stop(&mut self) {
        self.running = false;
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
    fn mock_stream_stops_and_clears_queue() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        let opener = MockVoiceCaptureOpener;
        let mut stream = opener.open_default(Arc::clone(&queue)).unwrap();
        assert!(stream.is_running());
        // inject via concrete type
        drop(stream);
        // After drop, a new mock can still open.
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
    fn i16_conversion_scales() {
        let src = [0_i16, 16384, -16384];
        let mut dst = Vec::new();
        i16_slice_to_f32(&src, &mut dst);
        assert!((dst[0] - 0.0).abs() < 1e-6);
        assert!((dst[1] - 0.5).abs() < 1e-4);
        assert!((dst[2] + 0.5).abs() < 1e-4);
    }
}
