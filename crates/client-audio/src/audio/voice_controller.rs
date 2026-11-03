//! Policy-driven local capture lifecycle.
//!
//! A Bevy system observes [`super::voice_input::VoiceInputPolicy`] and opens or stops the
//! microphone through [`super::voice_stream::VoiceCaptureOpener`]. Device work stays on the
//! capture owner thread; this module only issues start/stop decisions on the frame schedule.

use super::voice_input::{MicrophonePermission, VoiceInputPolicy};
use super::voice_permission::{outcome_from_error, outcome_from_success, permission_after_outcome};
use super::voice_pcm::BoundedPcmQueue;
use super::voice_stream::{
    MockVoiceCaptureOpener, SharedPcmQueue, VoiceCaptureError, VoiceCaptureOpener,
    VoiceCaptureQueues, VoiceCaptureStream,
};
use bevy::prelude::{ResMut, Resource};
use std::sync::Arc;

/// Owns the shared PCM queue and the optional running capture stream.
///
/// On native targets the default opener is CPAL; tests can inject
/// [`MockVoiceCaptureOpener`] through [`VoiceCaptureController::with_opener`].
#[derive(Resource)]
pub struct VoiceCaptureController {
    queue: SharedPcmQueue,
    monitor_queue: SharedPcmQueue,
    opener: Box<dyn VoiceCaptureOpener>,
    stream: Option<Box<dyn VoiceCaptureStream>>,
    last_capture_permitted: bool,
    last_error: Option<String>,
    open_attempts: u64,
    stop_count: u64,
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
            .is_some_and(|stream| stream.is_running())
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
        if !policy.enabled() || !policy.session_active() {
            return false;
        }
        match policy.permission() {
            MicrophonePermission::Granted | MicrophonePermission::Unknown => true,
            MicrophonePermission::Denied => false,
        }
    }

    /// Applies the current policy: open when capture may run, stop when not.
    ///
    /// Updates [`VoiceInputPolicy`] permission from the open outcome (probe-on-first-open).
    pub fn sync_with_policy(&mut self, policy: &mut VoiceInputPolicy) {
        let should_open = Self::should_open(policy);
        if should_open == self.last_capture_permitted && self.is_running() == should_open {
            if should_open && !self.is_running() {
                self.try_open(policy);
            }
            let accepting = policy.transmission_permitted();
        self.queue.set_accepting(accepting);
        self.monitor_queue.set_accepting(accepting);
            self.last_capture_permitted = should_open && self.is_running();
            return;
        }

        if should_open {
            self.try_open(policy);
        } else {
            self.force_stop();
        }
        // Only retain samples while the current activation mode permits transmission.
        // PTT keeps the device open but the callback queue is closed until the key is held.
        self.queue.set_accepting(policy.transmission_permitted());
        self.last_capture_permitted = should_open && self.is_running();
    }

    fn try_open(&mut self, policy: &mut VoiceInputPolicy) {
        if self.is_running() {
            return;
        }
        self.stream = None;
        self.open_attempts = self.open_attempts.saturating_add(1);
        match self.opener.open_default(VoiceCaptureQueues {
            transmit: Arc::clone(&self.queue),
            monitor: Arc::clone(&self.monitor_queue),
        }) {
            Ok(stream) => {
                self.last_error = None;
                self.stream = Some(stream);
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
            }
        }
    }

    /// Stops capture and retires queued PCM when the observed game session changes.
    pub fn reset_for_session_change(&mut self) {
        self.force_stop();
        self.last_capture_permitted = false;
    }

    fn force_stop(&mut self) {
        self.queue.set_accepting(false);
        if let Some(mut stream) = self.stream.take() {
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
        controller.queue().push(crate::audio::voice_pcm::PcmFrame::silence());
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

        policy.set_push_to_talk_held(true);
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert!(controller.queue().is_accepting());

        policy.set_push_to_talk_held(false);
        controller.sync_with_policy(&mut policy);
        assert!(controller.is_running());
        assert!(!controller.queue().is_accepting());
    }

    #[test]
    fn open_error_is_recorded() {
        struct FailingOpener;
        impl VoiceCaptureOpener for FailingOpener {
            fn open_default(
                &self,
                _queue: SharedPcmQueue,
            ) -> Result<Box<dyn VoiceCaptureStream>, VoiceCaptureError> {
                Err(VoiceCaptureError::NoDevice)
            }
        }

        let mut controller = VoiceCaptureController::with_opener(Box::new(FailingOpener));
        let mut policy = permitted_policy();
        controller.sync_with_policy(&mut policy);
        assert!(!controller.is_running());
        assert_eq!(controller.last_error(), Some("no input device available"));
        // Transient failure keeps Granted so the user can retry after plugging a device.
        assert_eq!(policy.permission(), MicrophonePermission::Granted);
    }
}
