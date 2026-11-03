//! Local capture → playback monitoring.
//!
//! [`VoiceLoopbackSource`] is a pull-driven rodio [`Source`] that drains
//! [`super::voice_pcm::BoundedPcmQueue`] into stereo frames at [`OUTPUT_RATE`].
//! It never blocks: an empty or closed queue yields silence (zeros) so the mixer
//! callback stays real-time safe. Attach with the existing named-audio
//! `play_raw` path when an `AudioDevice` is available; absence of a device is not
//! an error for Stage 1.

use super::voice::OUTPUT_RATE;
#[cfg(test)]
use super::voice_pcm::BoundedPcmQueue;
use super::voice_pcm::{VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};
use super::voice_stream::SharedPcmQueue;
use bevy::prelude::{NonSendMut, Res, ResMut, Resource};
use rodio::Source;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Pulls mono voice frames and emits interleaved stereo samples for the game mixer.
pub struct VoiceLoopbackSource {
    queue: SharedPcmQueue,
    /// Current frame samples (mono), or none when idle.
    current: Option<super::voice_pcm::PcmFrame>,
    index: usize,
    /// When true, emit zeros instead of pulling (local mute of monitor only).
    muted: Arc<std::sync::atomic::AtomicBool>,
    stopped: Arc<AtomicBool>,
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
            muted: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            gain: 1.0,
            pending_right: None,
        }
    }

    /// Shared mute flag for the monitor path (does not stop capture).
    #[must_use]
    pub fn mute_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.muted)
    }

    /// Shared cancellation flag; setting it ends the source on the next mixer pull.
    #[must_use]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopped)
    }

    pub fn set_gain(&mut self, gain: f32) {
        self.gain = if gain.is_finite() {
            gain.clamp(0.0, 2.0)
        } else {
            0.0
        };
    }

    fn pull_frame(&mut self) {
        self.current = self.queue.pop();
        self.index = 0;
    }
}

impl Iterator for VoiceLoopbackSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        // The monitor has a private queue, so mute may discard its backlog without
        // touching the transmit queue. Do not play buffered audio after a policy epoch.
        if self.muted.load(Ordering::Acquire) || !self.queue.is_accepting() {
            self.current = None;
            self.index = 0;
            self.pending_right = None;
            if !self.queue.is_empty() {
                self.queue.clear();
            }
            return Some(0.0);
        }
        if self
            .current
            .as_ref()
            .is_some_and(|frame| !frame.is_current_for(&self.queue))
        {
            self.current = None;
            self.index = 0;
            self.pending_right = None;
        }
        // Endless source: the mixer keeps us until the device is torn down.
        if let Some(right) = self.pending_right.take() {
            return Some(right);
        }
        if self.current.is_none() || self.index >= VOICE_FRAME_SAMPLES {
            self.pull_frame();
        }
        let mono = match &self.current {
            Some(frame) => {
                let sample = frame.samples()[self.index] * self.gain;
                self.index += 1;
                sample
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::voice_pcm::PcmFrame;

    #[test]
    fn attaches_to_the_existing_memory_mixer_without_opening_another_device() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        let mut frame = PcmFrame::silence();
        let mut samples = [0.0_f32; VOICE_FRAME_SAMPLES];
        samples[0] = 0.4;
        samples[1] = -0.2;
        frame.copy_from_slice(&samples);
        queue.push(frame);

        let (mut device, mut mixer) = crate::named_audio::AudioDevice::memory_mixer();
        assert!(device.play_source(VoiceLoopbackSource::new(queue)));
        assert!((mixer.next().unwrap() - 0.4).abs() < 1e-6);
        assert!((mixer.next().unwrap() - 0.4).abs() < 1e-6);
        assert!((mixer.next().unwrap() + 0.2).abs() < 1e-6);
        assert!((mixer.next().unwrap() + 0.2).abs() < 1e-6);
    }

    #[test]
    fn mock_capture_reaches_the_existing_mixer_without_draining_transmit_and_stops_cleanly() {
        use crate::audio::voice_stream::{MockVoiceCaptureStream, VoiceCaptureQueues};

        let transmit = Arc::new(BoundedPcmQueue::new(2));
        let monitor = Arc::new(BoundedPcmQueue::new(2));
        transmit.set_accepting(true);
        monitor.set_accepting(true);

        let mut capture = MockVoiceCaptureStream::new(VoiceCaptureQueues {
            transmit: Arc::clone(&transmit),
            monitor: Arc::clone(&monitor),
        });
        capture.inject_mono(&[0.25_f32; VOICE_FRAME_SAMPLES]);
        assert_eq!(transmit.len(), 1);
        assert_eq!(monitor.len(), 1);

        let mut source = VoiceLoopbackSource::new(Arc::clone(&monitor));
        let stopped = source.stop_flag();
        let (mut device, mut mixer) = crate::named_audio::AudioDevice::memory_mixer();
        assert!(device.play_source(source));
        assert!((mixer.next().unwrap() - 0.25).abs() < 1e-6);
        assert!((mixer.next().unwrap() - 0.25).abs() < 1e-6);

        // The monitor consumes only its private copy; the network queue is untouched.
        assert!(monitor.is_empty());
        assert_eq!(transmit.len(), 1);

        // Disconnect closes both gates, retires queued data, and cancels the mixer source.
        transmit.set_accepting(false);
        monitor.set_accepting(false);
        capture.stop();
        stopped.store(true, Ordering::Release);
        assert!(transmit.is_empty());
        assert!(monitor.is_empty());
        assert_eq!(mixer.next(), None);
    }

    #[test]
    fn disabled_audio_device_rejects_loopback_without_panicking() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        let mut device = crate::named_audio::AudioDevice::disabled();
        assert!(!device.play_source(VoiceLoopbackSource::new(queue)));
    }

    #[test]
    fn monitor_source_ends_when_capture_lifecycle_stops() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        let mut source = VoiceLoopbackSource::new(queue);
        source.stop_flag().store(true, Ordering::Release);
        assert_eq!(source.next(), None);
    }

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
    fn local_monitor_consumption_never_drains_transmit_queue() {
        let transmit = Arc::new(BoundedPcmQueue::new(2));
        let monitor = Arc::new(BoundedPcmQueue::new(2));
        transmit.set_accepting(true);
        monitor.set_accepting(true);
        let frame = PcmFrame::silence();
        transmit.push(frame);
        monitor.push(frame);

        let mut source = VoiceLoopbackSource::new(monitor.clone());
        let _ = source.next();
        let _ = source.next();
        assert!(monitor.is_empty());
        assert_eq!(transmit.len(), 1);
    }

    #[test]
    fn closed_gate_produces_silence_even_if_stale_slots_exist() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        queue.push(PcmFrame::silence());
        queue.set_accepting(false);
        let source = VoiceLoopbackSource::new(queue);
        let samples: Vec<f32> = source.take(4).collect();
        assert!(samples.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn active_frame_is_discarded_when_policy_epoch_changes() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        let mut frame = PcmFrame::silence();
        let mut samples = [0.0_f32; VOICE_FRAME_SAMPLES];
        samples[0] = 0.75;
        frame.copy_from_slice(&samples);
        queue.push(frame);

        let mut source = VoiceLoopbackSource::new(Arc::clone(&queue));
        assert!((source.next().unwrap() - 0.75).abs() < 1e-6);
        queue.set_accepting(false);
        queue.set_accepting(true);
        assert_eq!(source.next(), Some(0.0));
    }

    #[test]
    fn monitor_mute_clears_only_its_private_queue() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        queue.push(PcmFrame::silence());
        let mut source = VoiceLoopbackSource::new(Arc::clone(&queue));
        source.mute_flag().store(true, Ordering::Release);
        assert_eq!(source.next(), Some(0.0));
        assert!(queue.is_empty());
    }

    #[test]
    fn monitor_mute_suppresses_output_and_discards_private_backlog() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        let mut frame = PcmFrame::silence();
        let mut samples = [0.0_f32; VOICE_FRAME_SAMPLES];
        samples[0] = 1.0;
        frame.copy_from_slice(&samples);
        queue.push(frame);

        let mut source = VoiceLoopbackSource::new(Arc::clone(&queue));
        source
            .mute_flag()
            .store(true, std::sync::atomic::Ordering::Release);
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
    stopped: Option<Arc<AtomicBool>>,
    muted_requested: bool,
}

impl VoiceLoopbackAttachment {
    /// Mutes only local monitoring; capture and future network transmission are unchanged.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted_requested = muted;
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
    if !capture.is_running() {
        if let Some(stopped) = attachment.stopped.take() {
            stopped.store(true, Ordering::Release);
        }
        attachment.muted = None;
        attachment.attached = false;
        return;
    }
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
    let stopped = source.stop_flag();
    muted.store(attachment.muted_requested, Ordering::Release);
    if device.play_source(source) {
        attachment.muted = Some(muted);
        attachment.stopped = Some(stopped);
        attachment.attached = true;
    }
}
