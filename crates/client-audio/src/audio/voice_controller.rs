//! Policy-driven local capture lifecycle.
//!
//! A Bevy system observes [`super::voice_input::VoiceInputPolicy`] and opens or stops the
//! microphone through [`super::voice_stream::VoiceCaptureOpener`]. Device work stays on the
//! capture owner thread; this module only issues start/stop decisions on the frame schedule.

use super::voice_input::{MicrophonePermission, VoiceInputPolicy};
use super::voice_pcm::BoundedPcmQueue;
use super::voice_permission::{outcome_from_error, outcome_from_success, permission_after_outcome};
#[cfg(any(test, target_arch = "wasm32"))]
use super::voice_stream::MockVoiceCaptureOpener;
#[cfg(test)]
use super::voice_stream::VoiceCaptureError;
use super::voice_stream::{
    SharedPcmQueue, VoiceCaptureOpener, VoiceCaptureQueues, VoiceCaptureStream,
};
use bevy::prelude::{ResMut, Resource};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const INITIAL_OPEN_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_OPEN_RETRY_DELAY: Duration = Duration::from_secs(8);

/// Owns the shared PCM queue and the optional running capture stream.
///
/// On native targets the default opener is CPAL; tests can inject
/// [`MockVoiceCaptureOpener`] through [`VoiceCaptureController::with_opener`].
#[derive(Resource)]
pub struct VoiceCaptureController {
    queue: SharedPcmQueue,
    monitor_queue: SharedPcmQueue,
    opener: Box<dyn VoiceCaptureOpener>,
    stream: Option<Mutex<Box<dyn VoiceCaptureStream>>>,
    last_capture_permitted: bool,
    last_error: Option<String>,
    open_attempts: u64,
    stop_count: u64,
    next_open_attempt: Option<Instant>,
    open_retry_delay: Duration,
}

impl Default for VoiceCaptureController {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self::with_opener(Box::new(super::voice_stream::CpalVoiceCaptureOpener))
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self::with_opener(Box::new(MockVoiceCaptureOpener))
        }
    }
}

impl VoiceCaptureController {
    #[must_use]
    pub fn with_opener(opener: Box<dyn VoiceCaptureOpener>) -> Self {
        let queue = Arc::new(BoundedPcmQueue::with_default_capacity());
        let monitor_queue = Arc::new(BoundedPcmQueue::with_default_capacity());
        queue.set_accepting(false);
        monitor_queue.set_accepting(false);
        Self {
            queue,
            monitor_queue,
            opener,
            stream: None,
            last_capture_permitted: false,
            last_error: None,
            open_attempts: 0,
            stop_count: 0,
            next_open_attempt: None,
            open_retry_delay: INITIAL_OPEN_RETRY_DELAY,
        }
    }

    /// Updates capture gain for both the future network queue and local monitor.
    /// The percentage is bounded to 0..=200; 100 leaves microphone samples unchanged.
    pub fn set_input_sensitivity_percent(&self, percent: u8) {
        let gain = f32::from(percent.min(200)) / 100.0;
        self.queue.set_input_gain(gain);
        self.monitor_queue.set_input_gain(gain);
    }

    #[must_use]
    pub fn queue(&self) -> SharedPcmQueue {
        Arc::clone(&self.queue)
    }

    #[must_use]
    pub fn monitor_queue(&self) -> SharedPcmQueue {
        Arc::clone(&self.monitor_queue)
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|stream| stream.lock().is_ok_and(|stream| stream.is_running()))
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[must_use]
    pub fn open_attempts(&self) -> u64 {
        self.open_attempts
    }

    #[must_use]
    pub fn stop_count(&self) -> u64 {
        self.stop_count
    }

    /// Clears a denied permission result's retry deadline after the player changes OS privacy settings.
    ///
    /// The normal policy still decides whether an open attempt is allowed: voice must be
    /// enabled, the session active and the mode selected. This method does not open capture.
    pub fn retry_permission_probe(&mut self) {
        self.next_open_attempt = None;
        self.open_retry_delay = INITIAL_OPEN_RETRY_DELAY;
        self.last_error = None;
    }

    /// Requests a fresh local input-device snapshot from the active native worker.
    /// The returned descriptors arrive asynchronously through poll_microphone_events.
    pub fn refresh_microphone_devices(&mut self) -> Result<(), String> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "microphone capture worker is not active".to_owned())?;
        stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .refresh_input_devices()
    }

    /// Requests an exact selection from the latest device snapshot. A stale token is
    /// rejected by the worker; it is never treated as a request for the system default.
    pub fn select_microphone_device(
        &mut self,
        selection: super::voice_capture::MicrophoneSelection,
    ) -> Result<(), String> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "microphone capture worker is not active".to_owned())?;
        stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .select_input_device(selection)
    }

    /// Drains pending device-list, selection, and disconnect events without blocking.
    pub fn poll_microphone_events(&mut self) -> Vec<super::voice_capture::MicrophoneWorkerEvent> {
        let Some(stream) = self.stream.as_mut() else {
            return Vec::new();
        };
        let mut stream = stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut events = Vec::new();
        while let Some(event) = stream.try_recv_input_event() {
            events.push(event);
        }
        events
    }

    fn has_persistent_device_worker(&self) -> bool {
        self.stream.as_ref().is_some_and(|stream| {
            stream
                .lock()
                .is_ok_and(|stream| stream.has_persistent_device_worker())
        })
    }

    /// Whether the active native capture owner can enumerate and select local inputs.
    ///
    /// A persistent worker remains available after unplug so the user can refresh the
    /// catalog and select another device. Mock and browser streams do not expose CPAL
    /// device selection.
    #[must_use]
    pub fn supports_input_device_selection(&self) -> bool {
        self.has_persistent_device_worker()
    }

    /// Whether the controller should attempt to open the device for this policy.
    ///
    /// `Unknown` permission is allowed once as an OS probe when voice is enabled in a session.
    fn should_open(policy: &VoiceInputPolicy) -> bool {
        if !policy.enabled()
            || !policy.session_active()
            || policy.mode() == super::voice_input::MicrophoneMode::Unselected
        {
            return false;
        }
        match policy.permission() {
            MicrophonePermission::Granted | MicrophonePermission::Unknown => true,
            MicrophonePermission::Denied => false,
        }
    }

    /// Applies the current policy: open when capture may run, stop when not.
    ///
    /// Failed transient opens use an exponential retry delay (500 ms to 8 s) so a
    /// missing/hot-plugged device cannot trigger an open attempt every Bevy frame.
    /// The clock-aware helper keeps retry behavior deterministic in tests.
    pub fn sync_with_policy(&mut self, policy: &mut VoiceInputPolicy) {
        self.sync_with_policy_at(policy, Instant::now());
    }

    fn sync_with_policy_at(&mut self, policy: &mut VoiceInputPolicy, now: Instant) {
        let should_open = Self::should_open(policy);

        if !should_open {
            if self.stream.is_some() || self.last_capture_permitted {
                self.force_stop();
            } else {
                self.queue.set_accepting(false);
                self.monitor_queue.set_accepting(false);
                self.queue.clear();
                self.monitor_queue.clear();
            }
            self.next_open_attempt = None;
            self.open_retry_delay = INITIAL_OPEN_RETRY_DELAY;
            self.last_capture_permitted = false;
            return;
        }

        // The CPAL owner worker remains alive after a device disappears. Keep it
        // available so settings can refresh the snapshot and select another exact device;
        // reopening the OS default here would silently override the user's selection.
        if self.has_persistent_device_worker() && !self.is_running() {
            self.queue.set_accepting(false);
            self.monitor_queue.set_accepting(false);
            self.queue.clear();
            self.monitor_queue.clear();
            self.next_open_attempt = None;
            self.open_retry_delay = INITIAL_OPEN_RETRY_DELAY;
            self.last_capture_permitted = false;
            return;
        }

        // A legacy/non-worker stream can fail asynchronously (for example, a device
        // unplug or host error). Apply bounded retry rather than reopening every frame.
        if self.stream.is_some() && !self.is_running() {
            // A stream can fail before the frame loop first observes it as running.
            // Retire the dead handle immediately so this path is detected exactly once.
            self.stream = None;
            self.last_error = Some("microphone capture stream stopped unexpectedly".to_owned());
            self.next_open_attempt = Some(now + self.open_retry_delay);
            self.open_retry_delay = self
                .open_retry_delay
                .saturating_mul(2)
                .min(MAX_OPEN_RETRY_DELAY);
            self.last_capture_permitted = false;
        }

        if !self.is_running()
            && self
                .next_open_attempt
                .is_none_or(|deadline| now >= deadline)
        {
            self.try_open(policy, now);
        }

        // Keep both destinations in lockstep. The monitor has its own queue so its
        // playback consumer cannot steal frames from the future network sender.
        let accepting = self.is_running() && policy.transmission_permitted();
        self.queue.set_accepting(accepting);
        self.monitor_queue.set_accepting(accepting);
        self.last_capture_permitted = self.is_running();
    }

    fn try_open(&mut self, policy: &mut VoiceInputPolicy, now: Instant) {
        if self.is_running() {
            return;
        }
        self.stream = None;
        // Retire any samples left by a stream that stopped asynchronously before
        // attempting to reopen. The queues stay closed until the policy is re-applied.
        self.queue.set_accepting(false);
        self.monitor_queue.set_accepting(false);
        self.queue.clear();
        self.monitor_queue.clear();
        self.open_attempts = self.open_attempts.saturating_add(1);
        match self.opener.open_default(VoiceCaptureQueues {
            transmit: Arc::clone(&self.queue),
            monitor: Arc::clone(&self.monitor_queue),
        }) {
            Ok(stream) => {
                self.last_error = None;
                self.stream = Some(Mutex::new(stream));
                self.next_open_attempt = None;
                self.open_retry_delay = INITIAL_OPEN_RETRY_DELAY;
                policy.set_permission(permission_after_outcome(
                    policy.permission(),
                    outcome_from_success(),
                ));
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.stream = None;
                let outcome = outcome_from_error(&error);
                policy.set_permission(permission_after_outcome(policy.permission(), outcome));

                // Permission denial is terminal until the user/OS changes permission.
                // Other failures (no device, unsupported format, transient host errors)
                // are retried with bounded exponential backoff.
                if policy.permission() != MicrophonePermission::Denied {
                    self.next_open_attempt = Some(now + self.open_retry_delay);
                    self.open_retry_delay = self
                        .open_retry_delay
                        .saturating_mul(2)
                        .min(MAX_OPEN_RETRY_DELAY);
                } else {
                    self.next_open_attempt = None;
                }
            }
        }
    }

    /// Stops capture and retires queued PCM when the observed game session changes.
    pub fn reset_for_session_change(&mut self) {
        self.force_stop();
        self.last_capture_permitted = false;
        self.next_open_attempt = None;
        self.open_retry_delay = INITIAL_OPEN_RETRY_DELAY;
    }

    fn force_stop(&mut self) {
        self.queue.set_accepting(false);
        self.monitor_queue.set_accepting(false);
        if let Some(stream) = self.stream.take() {
            let mut stream = stream
                .into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            stream.stop();
            self.stop_count = self.stop_count.saturating_add(1);
        }
        self.queue.clear();
        self.monitor_queue.clear();
    }
}

/// Frame-schedule system: keep capture aligned with [`VoiceInputPolicy`].
pub fn sync_voice_capture_with_policy(
    mut policy: ResMut<VoiceInputPolicy>,
    mut controller: ResMut<VoiceCaptureController>,
) {
    controller.sync_with_policy(&mut policy);
}

/// Helper for tests and session adapters: mark multiplayer session active/inactive.
pub fn set_session_active_for_voice(policy: &mut VoiceInputPolicy, active: bool) {
    policy.set_session_active(active);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::voice_input::{MicrophoneMode, MicrophonePermission, VoiceInputPolicy};

    fn enabled_session() -> VoiceInputPolicy {
        let mut policy = VoiceInputPolicy::default();
        policy.set_enabled(true);
        policy.set_session_active(true);
        policy.set_mode(MicrophoneMode::OpenMicrophone);
        policy
    }

    fn permitted_policy() -> VoiceInputPolicy {
        let mut policy = enabled_session();
        policy.set_permission(MicrophonePermission::Granted);
        policy
    }

    fn test_controller() -> VoiceCaptureController {
        VoiceCaptureController::with_opener(Box::new(MockVoiceCaptureOpener))
    }

    #[test]
    fn opens_when_capture_becomes_permitted() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        assert!(!controller.is_running());
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert_eq!(controller.open_attempts(), 1);
    }

    #[test]
    fn unknown_permission_probes_and_becomes_granted_on_success() {
        let mut controller = test_controller();
        let mut policy = enabled_session();
        assert_eq!(policy.permission(), MicrophonePermission::Unknown);
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert_eq!(policy.permission(), MicrophonePermission::Granted);
    }

    #[test]
    fn unselected_mode_does_not_open_the_microphone() {
        let mut controller = test_controller();
        let mut policy = VoiceInputPolicy::default();
        policy.set_enabled(true);
        policy.set_session_active(true);
        policy.set_permission(MicrophonePermission::Granted);

        controller.sync_with_policy(&mut policy);

        assert!(!controller.is_running());
        assert_eq!(controller.open_attempts(), 0);
        assert!(!controller.queue().is_accepting());
        assert!(!controller.monitor_queue().is_accepting());
    }

    #[test]
    fn denied_permission_does_not_open() {
        let mut controller = test_controller();
        let mut policy = enabled_session();
        policy.set_permission(MicrophonePermission::Denied);
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
        assert_eq!(controller.open_attempts(), 0);
    }

    #[test]
    fn access_denied_open_marks_permission_denied() {
        struct DeniedOpener;
        impl VoiceCaptureOpener for DeniedOpener {
            fn open_default(
                &self,
                _queues: VoiceCaptureQueues,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                Err(VoiceCaptureError::Other("Access Denied".into()))
            }
        }

        let mut controller = VoiceCaptureController::with_opener(Box::new(DeniedOpener));
        let mut policy = enabled_session();
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
        assert_eq!(policy.permission(), MicrophonePermission::Denied);
    }

    #[test]
    fn stops_and_clears_queue_when_session_ends() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());

        policy.set_session_active(false);
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
        assert!(controller.queue().is_empty());
        assert_eq!(controller.stop_count(), 1);
    }

    #[test]
    fn session_reset_stops_stream_and_discards_queued_pcm() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        controller
            .queue()
            .push(crate::audio::voice_pcm::PcmFrame::silence());
        assert!(controller.is_running());
        assert_eq!(controller.queue().len(), 1);

        controller.reset_for_session_change();

        assert!(!controller.is_running());
        assert!(controller.queue().is_empty());
        assert_eq!(controller.stop_count(), 1);
    }

    #[test]
    fn permission_loss_stops_capture() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        policy.set_permission(MicrophonePermission::Denied);
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
        assert!(!policy.capture_permitted());
    }

    #[test]
    fn disable_voice_stops_without_requiring_mode_change() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        policy.set_enabled(false);
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
    }

    #[test]
    fn repeated_sync_while_permitted_does_not_reopen() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        controller.sync_with_policy(&mut policy);
        controller.sync_with_policy(&mut policy);
        assert_eq!(controller.open_attempts(), 1);
        assert!(controller.is_running());
    }

    #[test]
    fn push_to_talk_controls_pcm_queue_acceptance_without_closing_device() {
        let mut controller = test_controller();
        let mut policy = enabled_session();
        policy.set_mode(MicrophoneMode::PushToTalk);
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert!(!controller.queue().is_accepting());
        assert!(!controller.monitor_queue().is_accepting());

        policy.set_push_to_talk_held(false);
        policy.set_push_to_talk_held(true);
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert!(controller.queue().is_accepting());
        assert!(controller.monitor_queue().is_accepting());

        policy.set_push_to_talk_held(false);
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert!(!controller.queue().is_accepting());
        assert!(!controller.monitor_queue().is_accepting());
    }

    #[test]
    fn transient_open_failures_are_rate_limited_with_bounded_backoff() {
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingFailingOpener(Arc<AtomicU64>);
        impl VoiceCaptureOpener for CountingFailingOpener {
            fn open_default(
                &self,
                _queues: VoiceCaptureQueues,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Err(VoiceCaptureError::NoDevice)
            }
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let mut controller = VoiceCaptureController::with_opener(Box::new(CountingFailingOpener(
            Arc::clone(&attempts),
        )));
        let mut policy = permitted_policy();
        let start = std::time::Instant::now();

        controller.sync_with_policy_at(&mut policy, start);
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(499));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(500));
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        // The second failure doubles the delay to one second.
        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(1_499));
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(1_500));
        assert_eq!(attempts.load(Ordering::Relaxed), 3);

        // Repeated failures grow the delay but never exceed the eight-second cap.
        let mut now = start + Duration::from_secs(2);
        for _ in 0..8 {
            now += controller.open_retry_delay;
            controller.sync_with_policy_at(&mut policy, now);
        }
        assert_eq!(controller.open_retry_delay, MAX_OPEN_RETRY_DELAY);

        // A policy/session stop resets the retry window for the next deliberate start.
        let attempts_before_restart = attempts.load(Ordering::Relaxed);
        policy.set_session_active(false);
        controller.sync_with_policy_at(&mut policy, start + Duration::from_secs(2));
        policy.set_session_active(true);
        controller.sync_with_policy_at(&mut policy, start + Duration::from_secs(2));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            attempts_before_restart + 1
        );
    }

    #[test]
    fn unexpected_stream_stop_uses_bounded_retry_backoff() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        struct FailingStream(Arc<AtomicBool>);
        impl VoiceCaptureStream for FailingStream {
            fn stop(&mut self) {
                self.0.store(false, Ordering::Release);
            }

            fn is_running(&self) -> bool {
                self.0.load(Ordering::Acquire)
            }
        }

        struct RestartingOpener {
            running: Arc<AtomicBool>,
            attempts: Arc<AtomicU64>,
        }
        impl VoiceCaptureOpener for RestartingOpener {
            fn open_default(
                &self,
                _queues: VoiceCaptureQueues,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                self.running.store(true, Ordering::Release);
                Ok(Box::new(FailingStream(Arc::clone(&self.running))))
            }
        }

        let running = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicU64::new(0));
        let opener = RestartingOpener {
            running: Arc::clone(&running),
            attempts: Arc::clone(&attempts),
        };
        let mut controller = VoiceCaptureController::with_opener(Box::new(opener));
        let mut policy = permitted_policy();
        let start = Instant::now();

        controller.sync_with_policy_at(&mut policy, start);
        assert!(controller.is_running());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        // Simulate CPAL reporting an asynchronous device/host failure.
        running.store(false, Ordering::Release);
        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(1));
        assert!(!controller.is_running());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(
            controller.last_error(),
            Some("microphone capture stream stopped unexpectedly")
        );

        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(500));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(501));
        assert!(controller.is_running());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn stream_that_stops_before_first_observation_uses_retry_backoff() {
        use std::sync::atomic::{AtomicU64, Ordering};

        struct AlreadyStoppedStream;
        impl VoiceCaptureStream for AlreadyStoppedStream {
            fn stop(&mut self) {}
            fn is_running(&self) -> bool {
                false
            }
        }

        struct AlreadyStoppedOpener(Arc<AtomicU64>);
        impl VoiceCaptureOpener for AlreadyStoppedOpener {
            fn open_default(
                &self,
                _queues: VoiceCaptureQueues,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(AlreadyStoppedStream))
            }
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let opener = AlreadyStoppedOpener(Arc::clone(&attempts));
        let mut controller = VoiceCaptureController::with_opener(Box::new(opener));
        let mut policy = permitted_policy();
        let start = Instant::now();

        // The opener returns a handle that has already failed before the first
        // post-open is_running observation can mark it as permitted.
        controller.sync_with_policy_at(&mut policy, start);
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(
            controller.last_error(),
            Some("microphone capture stream stopped unexpectedly")
        );

        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(499));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        controller.sync_with_policy_at(&mut policy, start + Duration::from_millis(500));
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn persistent_worker_can_reselect_after_disconnect_without_default_fallback() {
        use std::collections::VecDeque;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        };

        #[derive(Clone)]
        struct Shared {
            running: Arc<AtomicBool>,
            opens: Arc<AtomicU64>,
            selections: Arc<Mutex<Vec<super::super::voice_capture::MicrophoneSelection>>>,
            events: Arc<Mutex<VecDeque<super::super::voice_capture::MicrophoneWorkerEvent>>>,
        }

        struct WorkerBackedStream(Shared);

        impl VoiceCaptureStream for WorkerBackedStream {
            fn stop(&mut self) {
                self.0.running.store(false, Ordering::Release);
            }

            fn is_running(&self) -> bool {
                self.0.running.load(Ordering::Acquire)
            }

            fn has_persistent_device_worker(&self) -> bool {
                true
            }

            fn refresh_input_devices(&mut self) -> Result<(), String> {
                self.0.events.lock().unwrap().push_back(
                    super::super::voice_capture::MicrophoneWorkerEvent::Devices(vec![
                        super::super::voice_capture::MicrophoneDevice {
                            id: "fresh-device-token".to_owned(),
                            name: "USB microphone".to_owned(),
                            is_default: false,
                        },
                    ]),
                );
                Ok(())
            }

            fn select_input_device(
                &mut self,
                selection: super::super::voice_capture::MicrophoneSelection,
            ) -> Result<(), String> {
                self.0.selections.lock().unwrap().push(selection.clone());
                self.0.running.store(true, Ordering::Release);
                let device_id = match selection {
                    super::super::voice_capture::MicrophoneSelection::Device(id) => id,
                    super::super::voice_capture::MicrophoneSelection::SystemDefault => {
                        "system-default".to_owned()
                    }
                };
                self.0.events.lock().unwrap().push_back(
                    super::super::voice_capture::MicrophoneWorkerEvent::Selected { device_id },
                );
                Ok(())
            }

            fn try_recv_input_event(
                &mut self,
            ) -> Option<super::super::voice_capture::MicrophoneWorkerEvent> {
                self.0.events.lock().unwrap().pop_front()
            }
        }

        struct WorkerBackedOpener(Shared);

        impl VoiceCaptureOpener for WorkerBackedOpener {
            fn open_default(
                &self,
                _queues: VoiceCaptureQueues,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                self.0.opens.fetch_add(1, Ordering::Relaxed);
                self.0.running.store(true, Ordering::Release);
                Ok(Box::new(WorkerBackedStream(self.0.clone())))
            }
        }

        let shared = Shared {
            running: Arc::new(AtomicBool::new(false)),
            opens: Arc::new(AtomicU64::new(0)),
            selections: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(VecDeque::new())),
        };
        let mut controller =
            VoiceCaptureController::with_opener(Box::new(WorkerBackedOpener(shared.clone())));
        let mut policy = permitted_policy();

        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert_eq!(shared.opens.load(Ordering::Relaxed), 1);

        shared.running.store(false, Ordering::Release);
        shared.events.lock().unwrap().push_back(
            super::super::voice_capture::MicrophoneWorkerEvent::DeviceUnavailable {
                device_id: Some("old-device-token".to_owned()),
            },
        );
        controller.sync_with_policy(&mut policy);

        // A worker-backed device loss pauses capture but retains the owner worker.
        assert!(!controller.is_running());
        assert_eq!(shared.opens.load(Ordering::Relaxed), 1);
        assert_eq!(
            controller.poll_microphone_events(),
            vec![
                super::super::voice_capture::MicrophoneWorkerEvent::DeviceUnavailable {
                    device_id: Some("old-device-token".to_owned())
                }
            ]
        );

        controller.refresh_microphone_devices().unwrap();
        assert_eq!(
            controller.poll_microphone_events(),
            vec![super::super::voice_capture::MicrophoneWorkerEvent::Devices(
                vec![super::super::voice_capture::MicrophoneDevice {
                    id: "fresh-device-token".to_owned(),
                    name: "USB microphone".to_owned(),
                    is_default: false,
                }]
            )]
        );

        controller
            .select_microphone_device(super::super::voice_capture::MicrophoneSelection::Device(
                "fresh-device-token".to_owned(),
            ))
            .unwrap();
        controller.sync_with_policy(&mut policy);

        assert!(controller.is_running());
        assert_eq!(shared.opens.load(Ordering::Relaxed), 1);
        assert_eq!(
            *shared.selections.lock().unwrap(),
            vec![super::super::voice_capture::MicrophoneSelection::Device(
                "fresh-device-token".to_owned()
            )]
        );
        assert_eq!(
            controller.poll_microphone_events(),
            vec![
                super::super::voice_capture::MicrophoneWorkerEvent::Selected {
                    device_id: "fresh-device-token".to_owned()
                }
            ]
        );
    }

    #[test]
    fn permission_retry_clears_previous_backoff_without_opening_capture() {
        let mut controller =
            VoiceCaptureController::with_opener(Box::new(MockVoiceCaptureOpener));
        controller.next_open_attempt = Some(Instant::now() + Duration::from_secs(8));
        controller.open_retry_delay = MAX_OPEN_RETRY_DELAY;
        controller.last_error = Some("permission denied".to_owned());

        controller.retry_permission_probe();

        assert!(controller.next_open_attempt.is_none());
        assert_eq!(controller.open_retry_delay, INITIAL_OPEN_RETRY_DELAY);
        assert_eq!(controller.last_error(), None);
        assert!(!controller.is_running());
    }

    #[test]
    fn input_sensitivity_updates_transmit_and_monitor_queues_together() {
        let controller = VoiceCaptureController::with_opener(Box::new(MockVoiceCaptureOpener));
        controller.set_input_sensitivity_percent(150);
        let queues = [controller.queue(), controller.monitor_queue()];
        let samples = [0.4_f32; 960];

        for queue in &queues {
            queue.set_accepting(true);
            queue.push_samples(&samples);
            let frame = queue.pop().expect("both queues should receive the frame");
            assert!((frame.samples()[0] - 0.6).abs() < 1e-6);
        }
    }

    #[test]
    fn open_error_is_recorded() {
        struct FailingOpener;
        impl VoiceCaptureOpener for FailingOpener {
            fn open_default(
                &self,
                _queues: VoiceCaptureQueues,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                Err(VoiceCaptureError::NoDevice)
            }
        }

        let mut controller = VoiceCaptureController::with_opener(Box::new(FailingOpener));
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
        assert_eq!(controller.last_error(), Some("no input device available"));
        assert!(!controller.queue().is_accepting());
        assert!(!controller.monitor_queue().is_accepting());
        // Transient failure keeps Granted so the user can retry after plugging a device.
        assert_eq!(policy.permission(), MicrophonePermission::Granted);
    }
}
