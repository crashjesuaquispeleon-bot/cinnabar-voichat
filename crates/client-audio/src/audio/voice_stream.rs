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

    /// Whether this stream keeps a persistent device-owner worker alive after unplug.
    fn has_persistent_device_worker(&self) -> bool {
        false
    }

    /// Refreshes the local device snapshot when the backend supports device selection.
    fn refresh_input_devices(&mut self) -> Result<(), String> {
        Err("microphone device refresh is unsupported".to_owned())
    }

    /// Selects an exact token from the most recently published device snapshot.
    fn select_input_device(
        &mut self,
        _selection: super::voice_capture::MicrophoneSelection,
    ) -> Result<(), String> {
        Err("microphone device selection is unsupported".to_owned())
    }

    /// Returns one pending local device event without blocking.
    fn try_recv_input_event(&mut self) -> Option<super::voice_capture::MicrophoneWorkerEvent> {
        None
    }
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
#[derive(Debug)]
pub struct MonoFrameBuilder {
    partial: [f32; VOICE_FRAME_SAMPLES],
    filled: usize,
}

impl Default for MonoFrameBuilder {
    fn default() -> Self {
        Self {
            partial: [0.0; VOICE_FRAME_SAMPLES],
            filled: 0,
        }
    }
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
            let sum = input_frame
                .iter()
                .map(|&sample| f32::from(sample))
                .sum::<f32>();
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
pub fn interleaved_to_mono_f32_into(src: &[f32], channels: u16, dst: &mut [f32]) -> Option<usize> {
    if channels == 0 || !src.len().is_multiple_of(channels as usize) {
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
        Self {
            running: true,
            queues,
            builder: MonoFrameBuilder::new(),
        }
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

/// Opens a CPAL microphone through the persistent owner-worker protocol.
/// The worker constructs the CPAL host, owns every Device and Stream, and processes
/// refresh/select/shutdown commands on that same thread.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct CpalVoiceCaptureOpener;

#[cfg(not(target_arch = "wasm32"))]
struct CpalMicrophoneWorkerBackend {
    host: rodio::cpal::Host,
    queues: VoiceCaptureQueues,
    running: Arc<std::sync::atomic::AtomicBool>,
    stream: Option<rodio::cpal::Stream>,
}

#[cfg(not(target_arch = "wasm32"))]
impl CpalMicrophoneWorkerBackend {
    /// Called only from the worker factory, so the host is born on its owner thread.
    fn new(queues: VoiceCaptureQueues, running: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            host: rodio::cpal::default_host(),
            queues,
            running,
            stream: None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl super::voice_capture::MicrophoneWorkerBackend for CpalMicrophoneWorkerBackend {
    type Device = rodio::cpal::Device;

    fn enumerate(&mut self) -> Result<Vec<(String, bool, Self::Device)>, String> {
        use rodio::cpal::traits::{DeviceTrait, HostTrait};

        let default_name = self
            .host
            .default_input_device()
            .and_then(|device| device.name().ok());
        let devices = self
            .host
            .input_devices()
            .map_err(|error| error.to_string())?
            .collect::<Vec<_>>();
        let names = devices
            .iter()
            .map(|device| device.name().unwrap_or_default())
            .collect::<Vec<_>>();

        // CPAL has no portable persistent ID here. A name is accepted as the default
        // only when it uniquely identifies one enumerated device.
        let unique_default = default_name.filter(|candidate| {
            !candidate.is_empty() && names.iter().filter(|name| *name == candidate).count() == 1
        });

        Ok(names
            .into_iter()
            .zip(devices)
            .enumerate()
            .map(|(index, (name, device))| {
                let is_default = unique_default.as_deref() == Some(name.as_str());
                let name = if name.is_empty() {
                    format!("Input device {}", index + 1)
                } else {
                    name
                };
                (name, is_default, device)
            })
            .collect())
    }

    fn stop(&mut self) {
        use std::sync::atomic::Ordering;

        self.running.store(false, Ordering::Release);
        self.queues.transmit.set_accepting(false);
        self.queues.monitor.set_accepting(false);
        // Drop the stream on the same thread that created it.
        drop(self.stream.take());
        self.queues.transmit.clear();
        self.queues.monitor.clear();
    }

    fn start(&mut self, device: &Self::Device) -> Result<(), String> {
        use rodio::cpal::traits::{DeviceTrait, StreamTrait};
        use std::sync::atomic::Ordering;

        self.stop();
        let supported = device
            .supported_input_configs()
            .map_err(|error| error.to_string())?
            .filter(|config| {
                config.min_sample_rate().0 <= VOICE_SAMPLE_RATE
                    && config.max_sample_rate().0 >= VOICE_SAMPLE_RATE
                    && matches!(
                        config.sample_format(),
                        rodio::cpal::SampleFormat::F32 | rodio::cpal::SampleFormat::I16
                    )
            })
            .max_by_key(|config| config.channels())
            .ok_or_else(|| "microphone does not support 48 kHz f32/i16 input".to_owned())?;

        let sample_format = supported.sample_format();
        let config = supported
            .with_sample_rate(rodio::cpal::SampleRate(VOICE_SAMPLE_RATE))
            .config();
        let channels = config.channels;

        let stream = match sample_format {
            rodio::cpal::SampleFormat::F32 => {
                let transmit = Arc::clone(&self.queues.transmit);
                let monitor = Arc::clone(&self.queues.monitor);
                let mut builder = MonoFrameBuilder::new();
                let callback_running = Arc::clone(&self.running);
                device
                    .build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            builder.push_interleaved_f32_to_queues(
                                data,
                                channels,
                                &[&transmit, &monitor],
                            );
                        },
                        move |_error| {
                            callback_running.store(false, Ordering::Release);
                        },
                        None,
                    )
                    .map_err(|error| error.to_string())?
            }
            rodio::cpal::SampleFormat::I16 => {
                let transmit = Arc::clone(&self.queues.transmit);
                let monitor = Arc::clone(&self.queues.monitor);
                let mut builder = MonoFrameBuilder::new();
                let callback_running = Arc::clone(&self.running);
                device
                    .build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            builder.push_interleaved_i16_to_queues(
                                data,
                                channels,
                                &[&transmit, &monitor],
                            );
                        },
                        move |_error| {
                            callback_running.store(false, Ordering::Release);
                        },
                        None,
                    )
                    .map_err(|error| error.to_string())?
            }
            _ => return Err("unsupported microphone sample format".to_owned()),
        };

        self.running.store(true, Ordering::Release);
        if let Err(error) = stream.play() {
            self.running.store(false, Ordering::Release);
            return Err(error.to_string());
        }
        self.stream = Some(stream);
        Ok(())
    }

    fn is_running(&self) -> bool {
        use std::sync::atomic::Ordering;

        self.stream.is_some() && self.running.load(Ordering::Acquire)
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct CpalVoiceCaptureHandle {
    worker: Option<super::voice_capture::MicrophoneWorker>,
    running: Arc<std::sync::atomic::AtomicBool>,
    queues: VoiceCaptureQueues,
}

#[cfg(not(target_arch = "wasm32"))]
impl VoiceCaptureStream for CpalVoiceCaptureHandle {
    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown();
        }
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        self.queues.transmit.set_accepting(false);
        self.queues.monitor.set_accepting(false);
        self.queues.transmit.clear();
        self.queues.monitor.clear();
    }

    fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }

    fn has_persistent_device_worker(&self) -> bool {
        self.worker.is_some()
    }

    fn refresh_input_devices(&mut self) -> Result<(), String> {
        self.worker
            .as_ref()
            .ok_or_else(|| "microphone owner worker has stopped".to_owned())?
            .send(super::voice_capture::MicrophoneWorkerCommand::Refresh)
            .map_err(|error| error.to_string())
    }

    fn select_input_device(
        &mut self,
        selection: super::voice_capture::MicrophoneSelection,
    ) -> Result<(), String> {
        self.worker
            .as_ref()
            .ok_or_else(|| "microphone owner worker has stopped".to_owned())?
            .send(super::voice_capture::MicrophoneWorkerCommand::Select(
                selection,
            ))
            .map_err(|error| error.to_string())
    }

    fn try_recv_input_event(
        &mut self,
    ) -> Option<super::voice_capture::MicrophoneWorkerEvent> {
        self.worker.as_ref()?.try_recv().ok()
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
        use super::voice_capture::{
            MicrophoneSelection, MicrophoneSelectionError, MicrophoneWorkerCommand,
            MicrophoneWorkerEvent, spawn_microphone_worker,
        };
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let running = Arc::new(AtomicBool::new(false));
        let worker_queues = queues.clone();
        let worker_running = Arc::clone(&running);
        let worker = spawn_microphone_worker(move || {
            CpalMicrophoneWorkerBackend::new(worker_queues, worker_running)
        })
        .map_err(|error| VoiceCaptureError::Other(error.to_string()))?;

        worker
            .send(MicrophoneWorkerCommand::Refresh)
            .map_err(|error| VoiceCaptureError::Other(error.to_string()))?;

        match worker.recv_timeout(Duration::from_secs(5)) {
            Ok(MicrophoneWorkerEvent::Devices(devices)) if !devices.is_empty() => {}
            Ok(MicrophoneWorkerEvent::Devices(_)) => {
                worker.shutdown();
                return Err(VoiceCaptureError::NoDevice);
            }
            Ok(MicrophoneWorkerEvent::BackendError(error)) => {
                worker.shutdown();
                return Err(VoiceCaptureError::Other(error));
            }
            Ok(_) => {
                worker.shutdown();
                return Err(VoiceCaptureError::Other(
                    "unexpected microphone discovery response".to_owned(),
                ));
            }
            Err(error) => {
                worker.shutdown();
                return Err(VoiceCaptureError::Other(error.to_string()));
            }
        }

        worker
            .send(MicrophoneWorkerCommand::Select(
                MicrophoneSelection::SystemDefault,
            ))
            .map_err(|error| VoiceCaptureError::Other(error.to_string()))?;

        match worker.recv_timeout(Duration::from_secs(10)) {
            Ok(MicrophoneWorkerEvent::Selected { .. })
                if running.load(std::sync::atomic::Ordering::Acquire) =>
            {
                Ok(Box::new(CpalVoiceCaptureHandle {
                    worker: Some(worker),
                    running,
                    queues,
                }))
            }
            Ok(MicrophoneWorkerEvent::SelectionRejected(
                MicrophoneSelectionError::NoDefaultDevice,
            )) => {
                worker.shutdown();
                Err(VoiceCaptureError::NoDevice)
            }
            Ok(MicrophoneWorkerEvent::SelectionRejected(error)) => {
                worker.shutdown();
                Err(VoiceCaptureError::Other(format!(
                    "microphone selection rejected: {error:?}"
                )))
            }
            Ok(MicrophoneWorkerEvent::BackendError(error)) => {
                worker.shutdown();
                Err(VoiceCaptureError::Other(error))
            }
            Ok(_) => {
                worker.shutdown();
                Err(VoiceCaptureError::Other(
                    "unexpected microphone open response".to_owned(),
                ))
            }
            Err(error) => {
                worker.shutdown();
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
        let stream = opener
            .open_default(VoiceCaptureQueues {
                transmit: Arc::clone(&shared),
                monitor: shared,
            })
            .unwrap();
        assert!(stream.is_running());
        drop(stream);
        let shared = Arc::clone(&queue);
        let mut stream = opener
            .open_default(VoiceCaptureQueues {
                transmit: Arc::clone(&shared),
                monitor: shared,
            })
            .unwrap();
        stream.stop();
        assert!(!stream.is_running());
    }

    #[test]
    fn mock_inject_fans_out_to_independent_queues() {
        let transmit = Arc::new(BoundedPcmQueue::new(4));
        let monitor = Arc::new(BoundedPcmQueue::new(4));
        transmit.set_accepting(true);
        monitor.set_accepting(true);
        let mut stream = MockVoiceCaptureStream::new(VoiceCaptureQueues {
            transmit: Arc::clone(&transmit),
            monitor: Arc::clone(&monitor),
        });
        let samples = vec![0.1_f32; VOICE_FRAME_SAMPLES];
        stream.inject_mono(&samples);
        assert_eq!(transmit.len(), 1);
        assert_eq!(monitor.len(), 1);
        assert!((transmit.pop().unwrap().samples()[0] - 0.1).abs() < 1e-6);
        assert!((monitor.pop().unwrap().samples()[0] - 0.1).abs() < 1e-6);
        stream.stop();
        assert!(transmit.is_empty());
        assert!(monitor.is_empty());
    }

    #[test]
    fn fixed_buffer_converters_do_not_require_vec_storage() {
        let stereo = [1.0_f32, 3.0, 5.0, 7.0];
        let mut mono = [0.0_f32; 2];
        assert_eq!(interleaved_to_mono_f32_into(&stereo, 2, &mut mono), Some(2));
        assert_eq!(mono, [2.0, 6.0]);
        assert_eq!(
            interleaved_to_mono_f32_into(&stereo, 2, &mut mono[..1]),
            None
        );
        assert_eq!(
            interleaved_to_mono_f32_into(&stereo[..3], 2, &mut mono),
            None
        );

        let integer = [0_i16, 16384, -16384];
        let mut float = [0.0_f32; 3];
        assert_eq!(i16_slice_to_f32_into(&integer, &mut float), Some(3));
        assert!((float[1] - 0.5).abs() < 1e-4);
        assert_eq!(i16_slice_to_f32_into(&integer, &mut float[..2]), None);
    }

    #[test]
    fn mock_stream_preserves_partial_frames_between_callbacks_for_both_destinations() {
        let transmit = Arc::new(BoundedPcmQueue::new(2));
        let monitor = Arc::new(BoundedPcmQueue::new(2));
        transmit.set_accepting(true);
        monitor.set_accepting(true);
        let mut stream = MockVoiceCaptureStream::new(VoiceCaptureQueues {
            transmit: Arc::clone(&transmit),
            monitor: Arc::clone(&monitor),
        });
        let first = vec![0.2_f32; VOICE_FRAME_SAMPLES / 2];
        let second = vec![0.4_f32; VOICE_FRAME_SAMPLES / 2];
        stream.inject_mono(&first);
        assert!(transmit.is_empty());
        assert!(monitor.is_empty());
        stream.inject_mono(&second);
        for queue in [&transmit, &monitor] {
            assert_eq!(queue.len(), 1);
            let frame = queue.pop().unwrap();
            assert!((frame.samples()[0] - 0.2).abs() < 1e-6);
            assert!((frame.samples()[VOICE_FRAME_SAMPLES / 2] - 0.4).abs() < 1e-6);
        }
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
