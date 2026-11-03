//! Policy-driven local capture lifecycle.
//!
//! A Bevy system observes [`super::voice_input::VoiceInputPolicy`] and opens or stops the
//! microphone through [`super::voice_stream::VoiceCaptureOpener`]. Device work stays on the
//! capture owner thread; this module only issues start/stop decisions on the frame schedule.

use super::voice_input::VoiceInputPolicy;
use super::voice_pcm::BoundedPcmQueue;
use super::voice_stream::{
    MockVoiceCaptureOpener, SharedPcmQueue, VoiceCaptureError, VoiceCaptureOpener,
    VoiceCaptureStream,
};
use bevy::prelude::{Res, ResMut, Resource};
use std::sync::Arc;

/// Owns the shared PCM queue and the optional running capture stream.
///
/// On native targets the default opener is CPAL; tests can inject
/// [`MockVoiceCaptureOpener`] through [`VoiceCaptureController::with_opener`].
#[derive(Resource)]
pub struct VoiceCaptureController {
    queue: SharedPcmQueue,
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
        Self {
            queue: Arc::new(BoundedPcmQueue::with_default_capacity()),
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

    /// Applies the current policy: open when capture becomes permitted, stop when not.
    pub fn sync_with_policy(&mut self, policy: &VoiceInputPolicy) {
        let permitted = policy.capture_permitted();
        if permitted == self.last_capture_permitted && self.is_running() == permitted {
            // Still re-check a dead stream while permitted so a device error can reopen later.
            if permitted && !self.is_running() {
                self.try_open();
            }
            self.last_capture_permitted = permitted;
            return;
        }

        if permitted {
            self.try_open();
        } else {
            self.force_stop();
        }
        self.last_capture_permitted = permitted;
    }

    fn try_open(&mut self) {
        if self.is_running() {
            return;
        }
        // Drop a stale non-running handle before opening again.
        self.stream = None;
        self.open_attempts = self.open_attempts.saturating_add(1);
        match self.opener.open_default(Arc::clone(&self.queue)) {
            Ok(stream) => {
                self.last_error = None;
                self.stream = Some(stream);
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.stream = None;
            }
        }
    }

    /// Stops capture and retires queued PCM when the observed game session changes.
    ///
    /// The app-side lifecycle adapter calls this before publishing the next session as active,
    /// so frames captured for the previous world cannot survive a transfer or reconnect.
    pub fn reset_for_session_change(&mut self) {
        self.force_stop();
        self.last_capture_permitted = false;
    }

    fn force_stop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.stop();
            self.stop_count = self.stop_count.saturating_add(1);
        }
        self.queue.clear();
    }
}

/// Frame-schedule system: keep capture aligned with [`VoiceInputPolicy`].
///
/// Must not perform CPAL work itself; open/stop delegate to the owner thread inside the opener.
pub fn sync_voice_capture_with_policy(
    policy: Res<VoiceInputPolicy>,
    mut controller: ResMut<VoiceCaptureController>,
) {
    controller.sync_with_policy(&policy);
}

/// Helper for tests and future session adapters: mark multiplayer session active/inactive.
pub fn set_session_active_for_voice(policy: &mut VoiceInputPolicy, active: bool) {
    policy.set_session_active(active);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::voice_input::{MicrophoneMode, MicrophonePermission, VoiceInputPolicy};

    fn permitted_policy() -> VoiceInputPolicy {
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(MicrophonePermission::Granted);
        policy.set_enabled(true);
        policy.set_session_active(true);
        policy.set_mode(MicrophoneMode::OpenMicrophone);
        policy
    }

    fn test_controller() -> VoiceCaptureController {
        VoiceCaptureController::with_opener(Box::new(MockVoiceCaptureOpener))
    }

    #[test]
    fn opens_when_capture_becomes_permitted() {
        let mut controller = test_controller();
        let policy = permitted_policy();
        assert!(!controller.is_running());
        controller.sync_with_policy(&policy);
        assert!(controller.is_running());
        assert_eq!(controller.open_attempts(), 1);
    }

    #[test]
    fn stops_and_clears_queue_when_session_ends() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&policy);
        assert!(controller.is_running());

        policy.set_session_active(false);
        controller.sync_with_policy(&policy);
        assert!(!controller.is_running());
        assert!(controller.queue().is_empty());
        assert_eq!(controller.stop_count(), 1);
    }

    #[test]
    fn permission_loss_stops_capture() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&policy);
        policy.set_permission(MicrophonePermission::Denied);
        controller.sync_with_policy(&policy);
        assert!(!controller.is_running());
        assert!(!policy.capture_permitted());
    }

    #[test]
    fn disable_voice_stops_without_requiring_mode_change() {
        let mut controller = test_controller();
        let mut policy = permitted_policy();
        controller.sync_with_policy(&policy);
        policy.set_enabled(false);
        controller.sync_with_policy(&policy);
        assert!(!controller.is_running());
    }

    #[test]
    fn repeated_sync_while_permitted_does_not_reopen() {
        let mut controller = test_controller();
        let policy = permitted_policy();
        controller.sync_with_policy(&policy);
        controller.sync_with_policy(&policy);
        controller.sync_with_policy(&policy);
        assert_eq!(controller.open_attempts(), 1);
        assert!(controller.is_running());
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
        let policy = permitted_policy();
        controller.sync_with_policy(&policy);
        assert!(!controller.is_running());
        assert_eq!(controller.last_error(), Some("no input device available"));
    }
}
