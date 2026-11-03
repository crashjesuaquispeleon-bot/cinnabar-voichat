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
