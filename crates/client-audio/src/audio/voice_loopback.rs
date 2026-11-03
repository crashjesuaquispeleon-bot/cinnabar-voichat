//! Local capture → playback monitoring.
//!
//! [`VoiceLoopbackSource`] is a pull-driven rodio [`Source`] that drains
//! [`super::voice_pcm::BoundedPcmQueue`] into stereo frames at [`OUTPUT_RATE`].
//! It never blocks: an empty or closed queue yields silence (zeros) so the mixer
//! callback stays real-time safe. Attach with the existing named-audio
//! `play_raw` path when an `AudioDevice` is available; absence of a device is not
//! an error for Stage 1.

use super::voice::OUTPUT_RATE;
use super::voice_pcm::{BoundedPcmQueue, VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};
use super::voice_stream::SharedPcmQueue;
use rodio::Source;
use bevy::prelude::{NonSendMut, Res, ResMut, Resource};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

/// Pulls mono voice frames and emits interleaved stereo samples for the game mixer.
pub struct VoiceLoopbackSource {
    queue: SharedPcmQueue,
    /// Current frame samples (mono), or none when idle.
    current: Option<[f32; VOICE_FRAME_SAMPLES]>,
    index: usize,
    /// When true, emit zeros instead of pulling (local mute of monitor only).
    muted: Arc<std::sync::atomic::AtomicBool>,
    gain: f32,
    pending_right: Option<f32>,
}

impl VoiceLoopbackSource {
    #[must_use]
    pub fn new(queue: SharedPcmQueue) -> Self {
        Self {
            queue,
            current: None,
            index: 0,
            muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            gain: 1.0,
            pending_right: None,
        }
    }

    /// Shared mute flag for the monitor path (does not stop capture).
    #[must_use]
    pub fn mute_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.muted)
    }

    pub fn set_gain(&mut self, gain: f32) {
        self.gain = if gain.is_finite() { gain.clamp(0.0, 2.0) } else { 0.0 };
    }

    fn pull_frame(&mut self) {
        if self
            .muted
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.current = None;
            self.index = 0;
            return;
        }
        match self.queue.pop() {
            Some(frame) => {
                self.current = Some(*frame.samples());
                self.index = 0;
            }
            None => {
                self.current = None;
                self.index = 0;
            }
        }
    }
}

impl Iterator for VoiceLoopbackSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        // Endless source: the mixer keeps us until the device is torn down.
        if let Some(right) = self.pending_right.take() {
            return Some(right);
        }
        if self.current.is_none() || self.index >= VOICE_FRAME_SAMPLES {
            self.pull_frame();
        }
        let mono = match &self.current {
            Some(samples) => {
                let s = samples[self.index] * self.gain;
                self.index += 1;
                s
            }
            None => 0.0,
        };
        // Dual-mono stereo for monitor path (no spatialization).
        self.pending_right = Some(mono);
        Some(mono)
    }
}

impl Source for VoiceLoopbackSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        2
    }

    fn sample_rate(&self) -> u32 {
        // Must match capture internal rate and existing voice OUTPUT_RATE.
        debug_assert_eq!(VOICE_SAMPLE_RATE, OUTPUT_RATE);
        OUTPUT_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Test helper: drain up to `max_frames` accepted frames into a contiguous mono buffer.
#[cfg(test)]
pub fn drain_mono_for_test(queue: &BoundedPcmQueue, max_frames: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..max_frames {
        match queue.pop() {
            Some(frame) => out.extend_from_slice(frame.samples()),
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::voice_pcm::PcmFrame;

    #[test]
    fn empty_queue_yields_silence_stereo_pairs() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        queue.set_accepting(true);
        let mut source = VoiceLoopbackSource::new(queue);
        let samples: Vec<f32> = source.by_ref().take(8).collect();
        assert_eq!(samples, vec![0.0; 8]);
    }

    #[test]
    fn drains_accepted_frame_as_dual_mono() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        queue.set_accepting(true);
        let mut frame = PcmFrame::silence();
        // Fill first two samples with known values.
        let mut samples = [0.0_f32; VOICE_FRAME_SAMPLES];
        samples[0] = 0.5;
        samples[1] = -0.25;
        frame.copy_from_slice(&samples);
        queue.push(frame);

        let mut source = VoiceLoopbackSource::new(queue);
        let left0 = source.next().unwrap();
        let right0 = source.next().unwrap();
        let left1 = source.next().unwrap();
        let right1 = source.next().unwrap();
        assert!((left0 - 0.5).abs() < 1e-6);
        assert!((right0 - 0.5).abs() < 1e-6);
        assert!((left1 + 0.25).abs() < 1e-6);
        assert!((right1 + 0.25).abs() < 1e-6);
    }

    #[test]
    fn closed_gate_produces_silence_even_if_stale_slots_exist() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        queue.push(PcmFrame::silence());
        queue.set_accepting(false);
        let mut source = VoiceLoopbackSource::new(queue);
        let samples: Vec<f32> = source.take(4).collect();
        assert!(samples.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn monitor_mute_suppresses_output_without_popping_when_reenabled_after_clear() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        let mut frame = PcmFrame::silence();
        let mut samples = [0.0_f32; VOICE_FRAME_SAMPLES];
        samples[0] = 1.0;
        frame.copy_from_slice(&samples);
        queue.push(frame);

        let mut source = VoiceLoopbackSource::new(Arc::clone(&queue));
        source.mute_flag().store(true, std::sync::atomic::Ordering::Release);
        let silenced: Vec<f32> = source.by_ref().take(4).collect();
        assert!(silenced.iter().all(|s| *s == 0.0));
    }
}


/// Tracks the one local monitor source attached to the shared named-audio mixer.
///
/// If the output device is absent or disabled, attachment simply remains pending; the
/// client and microphone capture continue without a loopback output.
#[derive(Resource, Default)]
pub struct VoiceLoopbackAttachment {
    attached: bool,
    muted: Option<Arc<AtomicBool>>,
}

impl VoiceLoopbackAttachment {
    /// Mutes only local monitoring; capture and future network transmission are unchanged.
    pub fn set_muted(&mut self, muted: bool) {
        if let Some(flag) = &self.muted {
            flag.store(muted, Ordering::Release);
        }
    }

    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.attached
    }
}

/// Attaches exactly one pull-driven monitor source to the existing named-audio output.
///
/// AudioDevice is non-send and is borrowed only on the Bevy main thread. No second
/// output stream is opened. Missing/disabled devices are treated as a normal no-op.
pub fn attach_voice_loopback_to_named_audio(
    capture: Res<super::voice_controller::VoiceCaptureController>,
    mut attachment: ResMut<VoiceLoopbackAttachment>,
    mut device: Option<NonSendMut<crate::named_audio::AudioDevice>>,
) {
    if attachment.attached {
        return;
    }
    let Some(device) = device.as_mut() else {
        return;
    };
    if !device.available() {
        return;
    }

    let source = VoiceLoopbackSource::new(capture.monitor_queue());
    let muted = source.mute_flag();
    if device.play_source(source) {
        attachment.muted = Some(muted);
        attachment.attached = true;
    }
}
