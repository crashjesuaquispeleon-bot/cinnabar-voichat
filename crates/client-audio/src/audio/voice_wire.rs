//! Initial voice media wire format for the native Go/WebRTC bridge.
//!
//! This is a small, bounded PCM profile for two-client validation. The SFU frame
//! is a big-endian sequence number followed by 20 ms of 16 kHz mono PCM16 audio.
//! Opus/RTP remains a later bandwidth and quality stage.

use super::voice_pcm::{PcmFrame, VOICE_FRAME_SAMPLES};

pub const VOICE_WIRE_SAMPLE_RATE: usize = 16_000;
pub const VOICE_WIRE_FRAME_SAMPLES: usize = 320;
pub const VOICE_WIRE_PCM_BYTES: usize = VOICE_WIRE_FRAME_SAMPLES * 2;
pub const VOICE_WIRE_SEQUENCE_BYTES: usize = 8;
pub const VOICE_WIRE_FRAME_BYTES: usize = VOICE_WIRE_SEQUENCE_BYTES + VOICE_WIRE_PCM_BYTES;

const DECIMATION: usize = 3;
const FILTER: [f32; 9] = [1.0, 2.0, 3.0, 4.0, 5.0, 4.0, 3.0, 2.0, 1.0];
const FILTER_SUM: f32 = 25.0;

/// Stateful 48 kHz to 16 kHz converter with a short low-pass FIR filter.
///
/// Eight samples of history preserve continuity across 20 ms capture frames.
/// Conversion is allocation-free and runs on the voice worker, never CPAL.
#[derive(Clone, Debug)]
pub struct Pcm16VoiceFrameEncoder {
    history: [f32; 8],
}

impl Default for Pcm16VoiceFrameEncoder {
    fn default() -> Self {
        Self { history: [0.0; 8] }
    }
}

impl Pcm16VoiceFrameEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Converts one 48 kHz mono frame into the exact 648-byte SFU payload.
    /// Sequence is unsigned 64-bit big-endian; samples are signed 16-bit little-endian.
    #[must_use]
    pub fn encode(&mut self, frame: &PcmFrame, sequence: u64) -> [u8; VOICE_WIRE_FRAME_BYTES] {
        let mut payload = [0_u8; VOICE_WIRE_FRAME_BYTES];
        payload[..VOICE_WIRE_SEQUENCE_BYTES].copy_from_slice(&sequence.to_be_bytes());
        let input = frame.samples();
        for output_index in 0..VOICE_WIRE_FRAME_SAMPLES {
            // A causal symmetric FIR adds only four 48 kHz samples of delay.
            let newest = output_index * DECIMATION;
            let mut filtered = 0.0_f32;
            for (tap, coefficient) in FILTER.iter().enumerate() {
                let index = newest as isize - tap as isize;
                let sample = if index < 0 {
                    self.history[(8 + index) as usize]
                } else {
                    input[index as usize]
                };
                let finite_sample = if sample.is_finite() { sample } else { 0.0 };
                filtered += finite_sample * coefficient;
            }
            let filtered = (filtered / FILTER_SUM).clamp(-1.0, 1.0);
            let pcm = if filtered <= -1.0 {
                i16::MIN
            } else {
                (filtered * f32::from(i16::MAX)).round() as i16
            };
            let offset = VOICE_WIRE_SEQUENCE_BYTES + output_index * 2;
            payload[offset..offset + 2].copy_from_slice(&pcm.to_le_bytes());
        }
        self.history
            .copy_from_slice(&input[VOICE_FRAME_SAMPLES - 8..]);
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_payload_has_sequence_and_exact_20ms_pcm_shape() {
        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(&[0.5; VOICE_FRAME_SAMPLES]);
        let mut encoder = Pcm16VoiceFrameEncoder::new();
        let _ = encoder.encode(&frame, 1);
        let payload = encoder.encode(&frame, 0x0102_0304_0506_0708);
        assert_eq!(payload.len(), 648);
        assert_eq!(&payload[..8], &0x0102_0304_0506_0708_u64.to_be_bytes());
        assert!(
            payload[8..]
                .chunks_exact(2)
                .all(|bytes| { i16::from_le_bytes([bytes[0], bytes[1]]) == 16_384 })
        );
    }

    #[test]
    fn decimation_filter_attenuates_48khz_nyquist_energy() {
        let alternating: Vec<f32> = (0..VOICE_FRAME_SAMPLES)
            .map(|index| if index % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(&alternating);
        let mut encoder = Pcm16VoiceFrameEncoder::new();
        let _ = encoder.encode(&frame, 1);
        let payload = encoder.encode(&frame, 2);
        let peak = payload[8..]
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs())
            .max()
            .unwrap_or_default();
        assert!(peak < 1_700, "Nyquist alias was not attenuated: {peak}");
    }

    #[test]
    fn non_finite_and_over_range_samples_are_safely_converted() {
        let mut samples = [2.0_f32; VOICE_FRAME_SAMPLES];
        samples[959] = f32::NAN;
        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(&samples);
        let mut encoder = Pcm16VoiceFrameEncoder::new();
        let _ = encoder.encode(&frame, 1);
        let payload = encoder.encode(&frame, 2);
        let pcm: Vec<i16> = payload[8..]
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();
        assert!(pcm.iter().any(|sample| *sample == i16::MAX));
        assert!(pcm.iter().all(|sample| *sample >= i16::MIN));
    }
}
