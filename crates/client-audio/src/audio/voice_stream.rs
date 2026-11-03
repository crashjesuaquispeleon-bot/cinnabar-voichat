//! Local microphone stream ownership boundary.
//!
//! The native CPAL opener owns the input stream on a dedicated thread and pushes fixed
//! frames into [`super::voice_pcm::BoundedPcmQueue`] without touching Bevy. The mock
//! implementation keeps lifecycle and conversion tests independent of physical devices.

use super::voice_pcm::{BoundedPcmQueue, PcmFrame, VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};
use std::sync::Arc;

/// Shared capture queue fed by the owner thread and drained by encode/playback.
pub type SharedPcmQueue = Arc<BoundedPcmQueue>;

/// Independent destinations fed by one capture callback.
#[derive(Clone)]
pub struct VoiceCaptureQueues {
    pub transmit: SharedPcmQueue,
    pub monitor: SharedPcmQueue,
}

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
        queues: VoiceCaptureQueues,
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
        self.push_mono_samples_to_queues(samples, &[queue]);
    }

    pub fn push_mono_samples_to_queues(&mut self, samples: &[f32], queues: &[&BoundedPcmQueue]) {
        let mut offset = 0;
        while offset < samples.len() {
            let space = VOICE_FRAME_SAMPLES - self.filled;
            let take = space.min(samples.len() - offset);
            self.partial[self.filled..self.filled + take]
                .copy_from_slice(&samples[offset..offset + take]);
            self.filled += take;
            offset += take;
            if self.filled == VOICE_FRAME_SAMPLES {
                self.emit_frame(queues);
            }
        }
    }

    /// Downmixes interleaved `f32` callback samples directly into fixed PCM frames.
    /// No temporary vector or heap allocation is used. Incomplete trailing channel data
    /// is ignored because it cannot form a complete interleaved sample frame.
    pub fn push_interleaved_f32(
        &mut self,
        samples: &[f32],
        channels: u16,
        queue: &BoundedPcmQueue,
    ) {
        self.push_interleaved_f32_to_queues(samples, channels, &[queue]);
    }

    pub fn push_interleaved_f32_to_queues(
        &mut self,
        samples: &[f32],
        channels: u16,
        queues: &[&BoundedPcmQueue],
    ) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }
        for input_frame in samples.chunks_exact(channels) {
            let mono = input_frame.iter().copied().sum::<f32>() / channels as f32;
            self.push_one(mono, queues);
        }
    }

    /// Converts and downmixes interleaved `i16` callback samples without allocating.
    pub fn push_interleaved_i16(
        &mut self,
        samples: &[i16],
        channels: u16,
        queue: &BoundedPcmQueue,
    ) {
        self.push_interleaved_i16_to_queues(samples, channels, &[queue]);
    }

    pub fn push_interleaved_i16_to_queues(
        &mut self,
        samples: &[i16],
        channels: u16,
        queues: &[&BoundedPcmQueue],
    ) {
        let channels = channels as usize;
        if channels == 0 {
            return;
        }
        for input_frame in samples.chunks_exact(channels) {
            let sum = input_frame.iter().map(|&sample| f32::from(sample)).sum::<f32>();
            self.push_one(sum / channels as f32 / 32768.0, queues);
        }
    }

    fn push_one(&mut self, sample: f32, queues: &[&BoundedPcmQueue]) {
        self.partial[self.filled] = sample;
        self.filled += 1;
        if self.filled == VOICE_FRAME_SAMPLES {
            self.emit_frame(queues);
        }
    }

    fn emit_frame(&mut self, queues: &[&BoundedPcmQueue]) {
        let mut frame = PcmFrame::silence();
        frame.copy_from_slice(&self.partial);
        for queue in queues {
            queue.push(frame);
        }
        self.filled = 0;
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
    queues: VoiceCaptureQueues,
    builder: MonoFrameBuilder,
}

impl MockVoiceCaptureStream {
    #[must_use]
    pub fn new(queues: VoiceCaptureQueues) -> Self {
        Self { running: true, queues, builder: MonoFrameBuilder::new() }
    }

    /// Injects mono samples as if a device callback had fired.
    pub fn inject_mono(&mut self, samples: &[f32]) {
        if self.running {
            self.builder.push_mono_samples_to_queues(
                samples,
                &[&self.queues.transmit, &self.queues.monitor],
            );
        }
    }
}

impl VoiceCaptureStream for MockVoiceCaptureStream {
    fn stop(&mut self) {
        self.running = false;
        self.builder.reset();
        self.queues.transmit.clear();
        self.queues.monitor.clear();
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
        queues: VoiceCaptureQueues,
    ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
        Ok(Box::new(MockVoiceCaptureStream::new(queues)))
    }
}


/// Opens the default CPAL microphone and owns its stream on a dedicated thread.
/// The device must expose an exact 48 kHz `f32` or `i16` input configuration;
/// unsupported rates are rejected rather than mislabeled as 48 kHz.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct CpalVoiceCaptureOpener;

#[cfg(not(target_arch = "wasm32"))]
struct CpalVoiceCaptureHandle {
    stop: Option<std::sync::mpsc::Sender<()>>,
    owner: Option<std::thread::JoinHandle<()>>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    queue: SharedPcmQueue,
}

#[cfg(not(target_arch = "wasm32"))]
impl VoiceCaptureStream for CpalVoiceCaptureHandle {
    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        // The owner thread has dropped the CPAL stream, so the callback cannot repopulate it.
        self.queue.clear();
    }

    fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for CpalVoiceCaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl VoiceCaptureOpener for CpalVoiceCaptureOpener {
    fn open_default(
        &self,
        queues: VoiceCaptureQueues,
    ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
        use rodio::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), VoiceCaptureError>>(1);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let running = std::sync::Arc::new(AtomicBool::new(false));
        let thread_running = std::sync::Arc::clone(&running);
        let thread_queues = queues.clone();

        let owner = std::thread::Builder::new()
            .name("voice-capture".to_owned())
            .spawn(move || {
                let host = rodio::cpal::default_host();
                let Some(device) = host.default_input_device() else {
                    let _ = ready_tx.send(Err(VoiceCaptureError::NoDevice));
                    return;
                };

                let supported = match device.supported_input_configs() {
                    Ok(configs) => configs
                        .filter(|config| {
                            config.min_sample_rate().0 <= VOICE_SAMPLE_RATE
                                && config.max_sample_rate().0 >= VOICE_SAMPLE_RATE
                                && matches!(
                                    config.sample_format(),
                                    rodio::cpal::SampleFormat::F32
                                        | rodio::cpal::SampleFormat::I16
                                )
                        })
                        .max_by_key(|config| config.channels()),
                    Err(error) => {
                        let _ = ready_tx.send(Err(VoiceCaptureError::Other(error.to_string())));
                        return;
                    }
                };

                let Some(supported) = supported else {
                    let _ = ready_tx.send(Err(VoiceCaptureError::UnsupportedFormat));
                    return;
                };
                let sample_format = supported.sample_format();
                let config = supported
                    .with_sample_rate(rodio::cpal::SampleRate(VOICE_SAMPLE_RATE))
                    .config();
                let channels = config.channels;

                let callback_running = std::sync::Arc::clone(&thread_running);
                let error_callback = move |_error| {
                    // Do not log or allocate in CPAL's real-time error callback.
                    callback_running.store(false, Ordering::Release);
                };

                let stream_result = match sample_format {
                    rodio::cpal::SampleFormat::F32 => {
                        let callback_transmit = Arc::clone(&thread_queues.transmit);
                        let callback_monitor = Arc::clone(&thread_queues.monitor);
                        let mut builder = MonoFrameBuilder::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _| {
                                builder.push_interleaved_f32_to_queues(
                                    data, channels, &[&callback_transmit, &callback_monitor],
                                );
                            },
                            error_callback,
                            None,
                        )
                    }
                    rodio::cpal::SampleFormat::I16 => {
                        let callback_queue = Arc::clone(&thread_queue);
                        let mut builder = MonoFrameBuilder::new();
                        let callback_running = std::sync::Arc::clone(&thread_running);
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _| {
                                builder.push_interleaved_i16_to_queues(
                                    data, channels, &[&callback_transmit, &callback_monitor],
                                );
                            },
                            move |_error| {
                                callback_running.store(false, Ordering::Release);
                            },
                            None,
                        )
                    }
                    _ => unreachable!("sample format was filtered above"),
                };

                let stream = match stream_result {
                    Ok(stream) => stream,
                    Err(error) => {
                        let _ = ready_tx.send(Err(VoiceCaptureError::Other(error.to_string())));
                        return;
                    }
                };
                thread_running.store(true, Ordering::Release);
                if let Err(error) = stream.play() {
                    thread_running.store(false, Ordering::Release);
                    let _ = ready_tx.send(Err(VoiceCaptureError::Other(error.to_string())));
                    return;
                }

                if ready_tx.send(Ok(())).is_err() {
                    thread_running.store(false, Ordering::Release);
                    drop(stream);
                    return;
                }

                loop {
                    match stop_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if !thread_running.load(Ordering::Acquire) {
                                break;
                            }
                        }
                    }
                }

                // Stream is dropped here, on the same owner thread that created it.
                drop(stream);
                thread_queues.transmit.clear();
                thread_queues.monitor.clear();
                thread_running.store(false, Ordering::Release);
            })
            .map_err(|error| VoiceCaptureError::Other(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(CpalVoiceCaptureHandle {
                stop: Some(stop_tx),
                owner: Some(owner),
                running,
                queues,
            })),
            Ok(Err(error)) => {
                let _ = owner.join();
                Err(error)
            }
            Err(error) => {
                let _ = owner.join();
                Err(VoiceCaptureError::Other(error.to_string()))
            }
        }
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
        queue.set_accepting(true);
        let mut builder = MonoFrameBuilder::new();
        let batch = vec![0.25_f32; VOICE_FRAME_SAMPLES * 2 + 10];
        builder.push_mono_samples(&batch, &queue);
        assert_eq!(queue.len(), 2);
        assert_eq!(builder.filled, 10);
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_none());
    }


    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn interleaved_builder_downmixes_without_temporary_vectors() {
        let queue = Arc::new(BoundedPcmQueue::new(2));
        queue.set_accepting(true);
        let mut builder = MonoFrameBuilder::new();
        let stereo = [0.2_f32, 0.4, 0.6, 0.8];
        builder.push_interleaved_f32(&stereo, 2, &queue);
        assert_eq!(builder.filled, 2);
        assert!(queue.is_empty());

        let silence = vec![0.0_f32; VOICE_FRAME_SAMPLES - 2];
        builder.push_mono_samples(&silence, &queue);
        let frame = queue.pop().unwrap();
        assert!((frame.samples()[0] - 0.3).abs() < 1e-6);
        assert!((frame.samples()[1] - 0.7).abs() < 1e-6);
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
        queue.set_accepting(true);
        let opener = MockVoiceCaptureOpener;
        let shared = Arc::clone(&queue);
        let stream = opener.open_default(VoiceCaptureQueues { transmit: Arc::clone(&shared), monitor: shared }).unwrap();
        assert!(stream.is_running());
        drop(stream);
        let shared = Arc::clone(&queue);
        let mut stream = opener.open_default(VoiceCaptureQueues { transmit: Arc::clone(&shared), monitor: shared }).unwrap();
        stream.stop();
        assert!(!stream.is_running());
    }

    #[test]
    fn mock_inject_fills_queue() {
        let queue = Arc::new(BoundedPcmQueue::new(4));
        queue.set_accepting(true);
        let shared = Arc::clone(&queue);
        let mut stream = MockVoiceCaptureStream::new(VoiceCaptureQueues { transmit: Arc::clone(&shared), monitor: shared });
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
        queue.set_accepting(true);
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
