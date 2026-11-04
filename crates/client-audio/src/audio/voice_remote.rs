//! Remote room voice receive queue and playback attachment.
//!
//! The Go/WebRTC client can clone this resource and call ingest_frame from its
//! networking worker. Frames are bounded, validated, converted to the existing
//! 48 kHz mono mixer format, and played through the one named-audio device.

use super::voice_loopback::VoiceLoopbackSource;
use super::voice_pcm::{BoundedPcmQueue, PcmFrame, VOICE_FRAME_SAMPLES};
use super::voice_stream::SharedPcmQueue;
use bevy::prelude::{NonSendMut, Res, ResMut, Resource};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const REMOTE_SAMPLE_RATE: usize = 16_000;
const REMOTE_FRAME_SAMPLES: usize = 320;
const REMOTE_SEQUENCE_BYTES: usize = 8;
const REMOTE_PCM_BYTES: usize = REMOTE_FRAME_SAMPLES * 2;
const REMOTE_FRAME_BYTES: usize = REMOTE_SEQUENCE_BYTES + REMOTE_PCM_BYTES;

/// A malformed remote payload or a frame that could not enter the bounded playback queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteVoiceFrameError {
    Disabled,
    WrongLength,
    QueueFull,
}

/// Thread-safe receive/playback handle. Clone it into the Go/WebRTC bridge worker;
/// no Bevy resource or device handle crosses the thread boundary.
#[derive(Resource, Clone)]
pub struct VoiceRemotePlayback {
    queue: SharedPcmQueue,
    enabled: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for VoiceRemotePlayback {
    fn default() -> Self {
        let queue = Arc::new(BoundedPcmQueue::with_default_capacity());
        queue.set_accepting(false);
        Self {
            queue,
            enabled: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl VoiceRemotePlayback {
    /// Turns remote playback on only while Voice Chat is enabled in a live session.
    pub fn set_voice_session_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        self.update_queue_gate();
    }

    /// Mutes incoming room audio locally without changing the microphone transmit gate.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
        self.update_queue_gate();
    }

    /// Sets the shared receive gain (0–200%). This is separate from microphone sensitivity.
    pub fn set_receive_volume_percent(&self, percent: u8) {
        self.queue
            .set_input_gain(f32::from(percent.min(200)) / 100.0);
    }

    fn update_queue_gate(&self) {
        let accepting = self.enabled.load(Ordering::Acquire)
            && !self.muted.load(Ordering::Acquire);
        self.queue.set_accepting(accepting);
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn queue(&self) -> SharedPcmQueue {
        Arc::clone(&self.queue)
    }

    /// Accepts one SFU frame: sequence number (8 bytes) + 320 little-endian PCM16 samples.
    ///
    /// Input is 16 kHz mono / 20 ms. It is linearly upsampled 3x into the client's
    /// 48 kHz mono frame queue. This method is safe on a network worker thread.
    pub fn ingest_frame(&self, payload: &[u8]) -> Result<(), RemoteVoiceFrameError> {
        if !self.queue.is_accepting() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::Disabled);
        }
        if payload.len() != REMOTE_FRAME_BYTES {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::WrongLength);
        }

        let mut source = [0.0_f32; REMOTE_FRAME_SAMPLES];
        for (index, bytes) in payload[REMOTE_SEQUENCE_BYTES..]
            .chunks_exact(2)
            .enumerate()
        {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            source[index] = f32::from(sample) / 32768.0;
        }

        let mut upsampled = [0.0_f32; VOICE_FRAME_SAMPLES];
        for index in 0..REMOTE_FRAME_SAMPLES {
            let current = source[index];
            let next = source.get(index + 1).copied().unwrap_or(current);
            let output = index * 3;
            upsampled[output] = current;
            upsampled[output + 1] = current + (next - current) / 3.0;
            upsampled[output + 2] = current + (next - current) * (2.0 / 3.0);
        }

        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(&upsampled);
        let dropped_before = self.queue.dropped();
        self.queue.push(frame);
        if self.queue.dropped() != dropped_before {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::QueueFull);
        }
        Ok(())
    }
}

/// Tracks the single remote playback source on the existing named-audio mixer.
#[derive(Resource, Default)]
pub struct VoiceRemoteAttachment {
    attached: bool,
    stopped: Option<Arc<AtomicBool>>,
}

impl VoiceRemoteAttachment {
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.attached
    }
}

/// Mirrors Voice Chat enablement/session state without requiring local microphone permission.
pub fn sync_remote_voice_playback(
    policy: Res<super::voice_input::VoiceInputPolicy>,
    playback: Res<VoiceRemotePlayback>,
) {
    let enabled = policy.enabled()
        && policy.session_active()
        && policy.mode() != super::voice_input::MicrophoneMode::Unselected;
    playback.set_voice_session_enabled(enabled);
}

/// Attach one remote source to the existing output device. The source is silenced and
/// detached on disable/session end; it never opens a separate OS output device.
pub fn attach_remote_voice_to_named_audio(
    playback: Res<VoiceRemotePlayback>,
    mut attachment: ResMut<VoiceRemoteAttachment>,
    mut device: Option<NonSendMut<crate::named_audio::AudioDevice>>,
) {
    if !playback.is_enabled() {
        if let Some(stopped) = attachment.stopped.take() {
            stopped.store(true, Ordering::Release);
        }
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
    let source = VoiceLoopbackSource::new(playback.queue());
    let stopped = source.stop_flag();
    if device.play_source(source) {
        attachment.stopped = Some(stopped);
        attachment.attached = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(sample: i16) -> Vec<u8> {
        let mut payload = vec![0_u8; REMOTE_FRAME_BYTES];
        payload[..REMOTE_SEQUENCE_BYTES].copy_from_slice(&1_u64.to_be_bytes());
        for sample_bytes in payload[REMOTE_SEQUENCE_BYTES..].chunks_exact_mut(2) {
            sample_bytes.copy_from_slice(&sample.to_le_bytes());
        }
        payload
    }

    #[test]
    fn remote_frames_are_gated_until_voice_is_enabled_for_a_session() {
        let playback = VoiceRemotePlayback::default();
        assert_eq!(
            playback.ingest_frame(&test_frame(16_384)),
            Err(RemoteVoiceFrameError::Disabled)
        );
        assert!(playback.queue().is_empty());

        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(16_384)), Ok(()));
        let frame = playback.queue().pop().expect("frame should enter the receive queue");
        assert!(frame.samples().iter().all(|sample| (*sample - 0.5).abs() < 0.0001));
    }

    #[test]
    fn remote_frame_length_is_strict_and_does_not_partially_play() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        let mut short = test_frame(8_192);
        short.pop();
        assert_eq!(
            playback.ingest_frame(&short),
            Err(RemoteVoiceFrameError::WrongLength)
        );
        assert!(playback.queue().is_empty());
    }

    #[test]
    fn incoming_mute_closes_and_clears_the_remote_queue() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(8_192)), Ok(()));
        assert!(!playback.queue().is_empty());

        playback.set_muted(true);
        assert!(playback.queue().is_empty());
        assert_eq!(
            playback.ingest_frame(&test_frame(8_192)),
            Err(RemoteVoiceFrameError::Disabled)
        );

        playback.set_muted(false);
        assert_eq!(playback.ingest_frame(&test_frame(8_192)), Ok(()));
    }

    #[test]
    fn receive_volume_is_independent_and_bounded() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        playback.set_receive_volume_percent(200);
        assert_eq!(playback.ingest_frame(&test_frame(16_384)), Ok(()));
        let frame = playback.queue().pop().expect("frame should be queued");
        assert!(frame.samples().iter().all(|sample| (*sample - 1.0).abs() < 0.0001));
    }

    #[test]
    fn playback_closes_when_voice_chat_session_is_disabled() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(4_096)), Ok(()));
        playback.set_voice_session_enabled(false);
        assert!(playback.queue().is_empty());
        assert_eq!(
            playback.ingest_frame(&test_frame(4_096)),
            Err(RemoteVoiceFrameError::Disabled)
        );
    }
}
