//! Remote room voice receive queues and concurrent playback mixing.
//!
//! Downlink frames carry a server-generated, opaque per-peer source ID, a per-source
//! sequence and one 20 ms mono PCM16 frame. Each source has an independent bounded
//! queue and startup prebuffer; the named-audio mixer sums active sources together.
//! No Bevy resource or device handle crosses the Go/WebRTC worker boundary.

use super::voice_pcm::{BoundedPcmQueue, PcmFrame, VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};
use super::voice_stream::SharedPcmQueue;
use bevy::prelude::{NonSendMut, Res, ResMut, Resource};
use rodio::Source;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const REMOTE_SAMPLE_RATE: usize = 16_000;
const REMOTE_FRAME_DURATION_MS: usize = 20;
const REMOTE_FRAME_SAMPLES: usize = REMOTE_SAMPLE_RATE * REMOTE_FRAME_DURATION_MS / 1000;
const REMOTE_UPSAMPLE_RATIO: usize = VOICE_SAMPLE_RATE as usize / REMOTE_SAMPLE_RATE;
pub const REMOTE_SOURCE_ID_BYTES: usize = 16;
const REMOTE_SEQUENCE_BYTES: usize = 8;
const REMOTE_PCM_BYTES: usize = REMOTE_FRAME_SAMPLES * 2;
/// Source ID + sequence + one 20 ms mono PCM16 packet, received from the SFU.
pub const REMOTE_FRAME_BYTES: usize =
    REMOTE_SOURCE_ID_BYTES + REMOTE_SEQUENCE_BYTES + REMOTE_PCM_BYTES;
/// Must match the SFU's maximum room participant limit.
pub const MAX_REMOTE_SOURCES: usize = 64;
const REMOTE_QUEUE_CAPACITY: usize = 5;
const REMOTE_STARTUP_BUFFER_FRAMES: usize = 2;
const REMOTE_SOURCE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

pub type RemoteQueues = Arc<[SharedPcmQueue; MAX_REMOTE_SOURCES]>;
type RemoteSourceSlots = [Option<RemoteSourceState>; MAX_REMOTE_SOURCES];

#[derive(Clone, Copy)]
struct RemoteSourceState {
    id: [u8; REMOTE_SOURCE_ID_BYTES],
    last_sequence: u64,
    last_seen: Instant,
    volume_percent: u8,
}

/// Opaque participant presentation data; it contains no account or XUID fields.
/// The source ID is random per SFU connection and changes when a participant reconnects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteVoiceParticipant {
    pub source_id: [u8; REMOTE_SOURCE_ID_BYTES],
    pub speaking: bool,
    pub volume_percent: u8,
}

/// A malformed, stale or unplayable remote payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteVoiceFrameError {
    Disabled,
    WrongLength,
    InvalidSourceID,
    StaleSequence,
    TooManySources,
    QueueFull,
}

/// Thread-safe receive/mix handle. Clone it into the Go/WebRTC bridge worker;
/// no Bevy resource or device handle crosses the thread boundary.
#[derive(Resource, Clone)]
pub struct VoiceRemotePlayback {
    queues: RemoteQueues,
    sources: Arc<Mutex<RemoteSourceSlots>>,
    enabled: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    master_volume: Arc<std::sync::atomic::AtomicU32>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for VoiceRemotePlayback {
    fn default() -> Self {
        let queues = Arc::new(std::array::from_fn(|_| {
            let queue = Arc::new(BoundedPcmQueue::new(REMOTE_QUEUE_CAPACITY));
            queue.set_accepting(false);
            queue
        }));
        Self {
            queues,
            sources: Arc::new(Mutex::new(std::array::from_fn(|_| None))),
            enabled: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            master_volume: Arc::new(std::sync::atomic::AtomicU32::new(1.0_f32.to_bits())),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl VoiceRemotePlayback {
    pub const REMOTE_FRAME_BYTES: usize = REMOTE_FRAME_BYTES;

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

    /// Sets the master receive gain (0–200%) without overwriting individual speaker levels.
    pub fn set_receive_volume_percent(&self, percent: u8) {
        let gain = f32::from(percent.min(200)) / 100.0;
        self.master_volume.store(gain.to_bits(), Ordering::Release);
        let sources = self.sources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (index, source) in sources.iter().enumerate() {
            if let Some(source) = source {
                self.queues[index].set_input_gain(gain * f32::from(source.volume_percent) / 100.0);
            }
        }
    }

    /// Sets one currently active speaker's local receive level (0–200%).
    /// The identifier is the SFU-generated ephemeral source ID, not a user/account ID.
    /// Returns false when that source is not present in the current room receive state.
    pub fn set_source_volume_percent(
        &self,
        source_id: [u8; REMOTE_SOURCE_ID_BYTES],
        percent: u8,
    ) -> bool {
        let percent = percent.min(200);
        let master = f32::from_bits(self.master_volume.load(Ordering::Acquire));
        let mut sources = self.sources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = sources
            .iter()
            .position(|source| source.is_some_and(|state| state.id == source_id))
        else {
            return false;
        };
        let source = sources[index].as_mut().expect("source index was found");
        source.volume_percent = percent;
        self.queues[index].set_input_gain(master * f32::from(percent) / 100.0);
        true
    }

    /// Returns recently heard participants for lightweight UI polling.
    /// A participant is considered speaking while a packet arrived in the last 300 ms;
    /// an idle source is removed from this snapshot after the five-second expiry window.
    pub fn active_participants(&self) -> Vec<RemoteVoiceParticipant> {
        let now = Instant::now();
        let sources = self.sources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sources
            .iter()
            .filter_map(|source| {
                let state = (*source)?;
                let age = now.duration_since(state.last_seen);
                (age < REMOTE_SOURCE_IDLE_TIMEOUT).then_some(RemoteVoiceParticipant {
                    source_id: state.id,
                    speaking: age <= Duration::from_millis(300),
                    volume_percent: state.volume_percent,
                })
            })
            .collect()
    }

    fn update_queue_gate(&self) {
        let accepting = self.enabled.load(Ordering::Acquire) && !self.muted.load(Ordering::Acquire);
        for queue in self.queues.iter() {
            queue.set_accepting(accepting);
        }
        if !accepting {
            let mut sources = self.sources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sources.fill(None);
        }
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
    pub fn is_empty(&self) -> bool {
        self.queues.iter().all(|queue| queue.is_empty())
    }

    #[must_use]
    pub fn mixer_queues(&self) -> RemoteQueues {
        Arc::clone(&self.queues)
    }

    fn resolve_source(
        &self,
        source_id: [u8; REMOTE_SOURCE_ID_BYTES],
        sequence: u64,
    ) -> Result<usize, RemoteVoiceFrameError> {
        let now = Instant::now();
        let mut sources = self.sources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(index) = sources
            .iter()
            .position(|source| source.is_some_and(|state| state.id == source_id))
        {
            let state = sources[index].as_mut().expect("source index was found");
            if sequence <= state.last_sequence {
                return Err(RemoteVoiceFrameError::StaleSequence);
            }
            state.last_sequence = sequence;
            state.last_seen = now;
            return Ok(index);
        }

        let index = sources
            .iter()
            .position(Option::is_none)
            .or_else(|| {
                sources.iter().position(|source| {
                    source.is_some_and(|state| now.duration_since(state.last_seen) >= REMOTE_SOURCE_IDLE_TIMEOUT)
                })
            })
            .ok_or(RemoteVoiceFrameError::TooManySources)?;

        // Recycle a silent/disconnected participant's slot after five seconds. Toggling
        // its queue gate advances the queue generation and invalidates an in-flight frame
        // already held by the audio mixer.
        if sources[index].is_some() {
            self.queues[index].set_accepting(false);
            self.queues[index].set_accepting(
                self.enabled.load(Ordering::Acquire) && !self.muted.load(Ordering::Acquire),
            );
        }
        sources[index] = Some(RemoteSourceState {
            id: source_id,
            last_sequence: sequence,
            last_seen: now,
            volume_percent: 100,
        });
        let master = f32::from_bits(self.master_volume.load(Ordering::Acquire));
        self.queues[index].set_input_gain(master);
        Ok(index)
    }

    /// Accepts an SFU downlink: 16-byte opaque source ID, 8-byte big-endian sequence,
    /// then 320 little-endian PCM16 samples. The sequence is checked independently
    /// for each source, so every participant may begin at sequence one.
    ///
    /// Input is 16 kHz mono / 20 ms. It is linearly upsampled 3x into the selected
    /// participant's 48 kHz queue. This method is safe on a network worker thread.
    pub fn ingest_frame(&self, payload: &[u8]) -> Result<(), RemoteVoiceFrameError> {
        if !self.enabled.load(Ordering::Acquire) || self.muted.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::Disabled);
        }
        if payload.len() != REMOTE_FRAME_BYTES {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::WrongLength);
        }

        let source_id: [u8; REMOTE_SOURCE_ID_BYTES] = payload[..REMOTE_SOURCE_ID_BYTES]
            .try_into()
            .expect("source ID length was validated");
        if source_id.iter().all(|byte| *byte == 0) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::InvalidSourceID);
        }
        let sequence_start = REMOTE_SOURCE_ID_BYTES;
        let sequence_end = sequence_start + REMOTE_SEQUENCE_BYTES;
        let sequence = u64::from_be_bytes(
            payload[sequence_start..sequence_end]
                .try_into()
                .expect("sequence length was validated"),
        );
        if sequence == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(RemoteVoiceFrameError::StaleSequence);
        }
        let source_index = match self.resolve_source(source_id, sequence) {
            Ok(index) => index,
            Err(error) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };

        let mut source = [0.0_f32; REMOTE_FRAME_SAMPLES];
        for (index, bytes) in payload[sequence_end..].chunks_exact(2).enumerate() {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            source[index] = f32::from(sample) / 32768.0;
        }

        let mut upsampled = [0.0_f32; VOICE_FRAME_SAMPLES];
        for index in 0..REMOTE_FRAME_SAMPLES {
            let current = source[index];
            let next = source.get(index + 1).copied().unwrap_or(current);
            let output = index * REMOTE_UPSAMPLE_RATIO;
            for step in 0..REMOTE_UPSAMPLE_RATIO {
                let fraction = step as f32 / REMOTE_UPSAMPLE_RATIO as f32;
                upsampled[output + step] = current + (next - current) * fraction;
            }
        }

        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(&upsampled);
        let queue = &self.queues[source_index];
        let pushed_before = queue.pushed();
        let dropped_before = queue.dropped();
        queue.push(frame);
        if queue.pushed() == pushed_before {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return if queue.dropped() != dropped_before {
                Err(RemoteVoiceFrameError::QueueFull)
            } else {
                Err(RemoteVoiceFrameError::Disabled)
            };
        }
        Ok(())
    }
}

/// Pull-driven multi-source mixer attached to the existing named-audio output.
/// It waits for two frames at each source's start/restart to absorb short scheduling
/// jitter, then mixes every active participant on the same output timeline.
pub struct VoiceRemoteMixerSource {
    queues: RemoteQueues,
    current: [Option<PcmFrame>; MAX_REMOTE_SOURCES],
    indices: [usize; MAX_REMOTE_SOURCES],
    started: [bool; MAX_REMOTE_SOURCES],
    stopped: Arc<AtomicBool>,
    pending_right: Option<f32>,
}

impl VoiceRemoteMixerSource {
    #[must_use]
    pub fn new(queues: RemoteQueues) -> Self {
        Self {
            queues,
            current: [None; MAX_REMOTE_SOURCES],
            indices: [0; MAX_REMOTE_SOURCES],
            started: [false; MAX_REMOTE_SOURCES],
            stopped: Arc::new(AtomicBool::new(false)),
            pending_right: None,
        }
    }

    /// Shared cancellation flag; setting it ends the source on the next mixer pull.
    #[must_use]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopped)
    }
}

impl Iterator for VoiceRemoteMixerSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        if let Some(right) = self.pending_right.take() {
            return Some(right);
        }

        let mut mixed = 0.0_f32;
        for index in 0..MAX_REMOTE_SOURCES {
            let queue = &self.queues[index];
            if !queue.is_accepting() {
                self.current[index] = None;
                self.indices[index] = 0;
                self.started[index] = false;
                continue;
            }
            if self.current[index]
                .as_ref()
                .is_some_and(|frame| !frame.is_current_for(queue))
            {
                self.current[index] = None;
                self.indices[index] = 0;
                self.started[index] = false;
            }
            if self.current[index].is_none() || self.indices[index] >= VOICE_FRAME_SAMPLES {
                self.current[index] = None;
                self.indices[index] = 0;
                if !self.started[index] && queue.len() < REMOTE_STARTUP_BUFFER_FRAMES {
                    continue;
                }
                self.started[index] = true;
                self.current[index] = queue.pop();
                if self.current[index].is_none() {
                    self.started[index] = false;
                    continue;
                }
            }
            if let Some(frame) = self.current[index].as_ref() {
                mixed += frame.samples()[self.indices[index]];
                self.indices[index] += 1;
            }
        }

        let mono = mixed.clamp(-1.0, 1.0);
        self.pending_right = Some(mono);
        Some(mono)
    }
}

impl Source for VoiceRemoteMixerSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        2
    }

    fn sample_rate(&self) -> u32 {
        VOICE_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Tracks the single mixed remote source on the existing named-audio output.
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

/// Attach one multi-speaker source to the existing output device. It never opens a
/// separate OS output device; disable/session end cancels the shared mixer source.
pub fn attach_remote_voice_to_named_audio(
    playback: Res<VoiceRemotePlayback>,
    mut attachment: ResMut<VoiceRemoteAttachment>,
    mut device: Option<NonSendMut<crate::named_audio::AudioDevice>>,
) {
    if !playback.is_enabled() || playback.is_muted() {
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
    let source = VoiceRemoteMixerSource::new(playback.mixer_queues());
    let stopped = source.stop_flag();
    if device.play_source(source) {
        attachment.stopped = Some(stopped);
        attachment.attached = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(source_id: u8, sequence: u64, sample: i16) -> Vec<u8> {
        let mut payload = vec![0_u8; REMOTE_FRAME_BYTES];
        payload[..REMOTE_SOURCE_ID_BYTES].fill(source_id);
        let sequence_start = REMOTE_SOURCE_ID_BYTES;
        payload[sequence_start..sequence_start + REMOTE_SEQUENCE_BYTES]
            .copy_from_slice(&sequence.to_be_bytes());
        for sample_bytes in payload[sequence_start + REMOTE_SEQUENCE_BYTES..].chunks_exact_mut(2) {
            sample_bytes.copy_from_slice(&sample.to_le_bytes());
        }
        payload
    }

    #[test]
    fn remote_frames_are_gated_until_voice_is_enabled_for_a_session() {
        let playback = VoiceRemotePlayback::default();
        assert_eq!(
            playback.ingest_frame(&test_frame(1, 1, 16_384)),
            Err(RemoteVoiceFrameError::Disabled)
        );
        assert!(playback.is_empty());

        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 16_384)), Ok(()));
        let frame = playback.mixer_queues()[0]
            .pop()
            .expect("frame should enter the speaker queue");
        assert!(
            frame
                .samples()
                .iter()
                .all(|sample| (*sample - 0.5).abs() < 0.0001)
        );
    }

    #[test]
    fn remote_frame_length_and_source_id_are_strict() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        let mut short = test_frame(1, 1, 8_192);
        short.pop();
        assert_eq!(
            playback.ingest_frame(&short),
            Err(RemoteVoiceFrameError::WrongLength)
        );
        assert_eq!(
            playback.ingest_frame(&test_frame(0, 1, 8_192)),
            Err(RemoteVoiceFrameError::InvalidSourceID)
        );
        assert!(playback.is_empty());
    }

    #[test]
    fn incoming_mute_closes_and_clears_every_speaker_queue() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 8_192)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(2, 1, 8_192)), Ok(()));
        assert!(!playback.is_empty());

        playback.set_muted(true);
        assert!(playback.is_empty());
        assert_eq!(
            playback.ingest_frame(&test_frame(1, 2, 8_192)),
            Err(RemoteVoiceFrameError::Disabled)
        );

        playback.set_muted(false);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 8_192)), Ok(()));
    }

    #[test]
    fn duplicate_and_out_of_order_sequences_are_scoped_to_each_speaker() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 8_192)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 8_192)), Err(RemoteVoiceFrameError::StaleSequence));
        assert_eq!(playback.ingest_frame(&test_frame(2, 1, 16_384)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(1, 2, 8_192)), Ok(()));
    }

    #[test]
    fn multiple_speakers_are_mixed_simultaneously_instead_of_played_in_turns() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);

        // Two frames per sender satisfy the 40 ms startup prebuffer.
        for sequence in [1, 2] {
            assert_eq!(playback.ingest_frame(&test_frame(1, sequence, 8_192)), Ok(()));
            assert_eq!(playback.ingest_frame(&test_frame(2, sequence, 16_384)), Ok(()));
        }

        let mut source = VoiceRemoteMixerSource::new(playback.mixer_queues());
        assert!((source.next().expect("left mix sample") - 0.75).abs() < 0.0001);
        assert!((source.next().expect("right mix sample") - 0.75).abs() < 0.0001);
    }

    #[test]
    fn receive_volume_is_independent_and_bounded() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        playback.set_receive_volume_percent(200);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 16_384)), Ok(()));
        let frame = playback.mixer_queues()[0].pop().expect("frame should be queued");
        assert!(
            frame
                .samples()
                .iter()
                .all(|sample| (*sample - 1.0).abs() < 0.0001)
        );
    }

    #[test]
    fn individual_speaker_volume_does_not_change_other_sources_or_master_volume() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 8_192)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(2, 1, 16_384)), Ok(()));
        let queues = playback.mixer_queues();
        let _ = queues[0].pop().expect("first speaker initial frame");
        let _ = queues[1].pop().expect("second speaker initial frame");

        assert!(playback.set_source_volume_percent([1; REMOTE_SOURCE_ID_BYTES], 0));
        assert!(playback.set_source_volume_percent([2; REMOTE_SOURCE_ID_BYTES], 200));
        assert_eq!(playback.ingest_frame(&test_frame(1, 2, 8_192)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(2, 2, 16_384)), Ok(()));

        let first = queues[0].pop().expect("muted speaker frame");
        let second = queues[1].pop().expect("boosted speaker frame");
        assert!(first.samples().iter().all(|sample| *sample == 0.0));
        assert!(second.samples().iter().all(|sample| (*sample - 1.0).abs() < 0.0001));

        playback.set_receive_volume_percent(50);
        assert_eq!(playback.ingest_frame(&test_frame(1, 3, 8_192)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(2, 3, 16_384)), Ok(()));
        let first = queues[0].pop().expect("master-scaled muted speaker frame");
        let second = queues[1].pop().expect("master-scaled boosted speaker frame");
        assert!(first.samples().iter().all(|sample| *sample == 0.0));
        assert!(second.samples().iter().all(|sample| (*sample - 0.5).abs() < 0.0001));
    }

    #[test]
    fn activity_snapshot_contains_only_ephemeral_source_ids_and_volume_controls() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(7, 1, 8_192)), Ok(()));
        assert_eq!(playback.ingest_frame(&test_frame(8, 1, 8_192)), Ok(()));
        assert!(playback.set_source_volume_percent([8; REMOTE_SOURCE_ID_BYTES], 135));

        let participants = playback.active_participants();
        assert_eq!(participants.len(), 2);
        assert!(participants.iter().all(|participant| participant.speaking));
        assert!(participants.iter().any(|participant| {
            participant.source_id == [7; REMOTE_SOURCE_ID_BYTES]
                && participant.volume_percent == 100
        }));
        assert!(participants.iter().any(|participant| {
            participant.source_id == [8; REMOTE_SOURCE_ID_BYTES]
                && participant.volume_percent == 135
        }));
    }

    #[test]
    fn playback_closes_when_voice_chat_session_is_disabled() {
        let playback = VoiceRemotePlayback::default();
        playback.set_voice_session_enabled(true);
        assert_eq!(playback.ingest_frame(&test_frame(1, 1, 4_096)), Ok(()));
        playback.set_voice_session_enabled(false);
        assert!(playback.is_empty());
        assert_eq!(
            playback.ingest_frame(&test_frame(1, 2, 4_096)),
            Err(RemoteVoiceFrameError::Disabled)
        );
    }

    #[test]
    fn mixer_source_ends_when_voice_lifecycle_stops() {
        let playback = VoiceRemotePlayback::default();
        let mut source = VoiceRemoteMixerSource::new(playback.mixer_queues());
        source.stop_flag().store(true, Ordering::Release);
        assert_eq!(source.next(), None);
    }
}
