//! Privacy and lifecycle policy for local voice capture.
//!
//! This module deliberately owns no device handle or worker thread. Platform capture is
//! connected only after its permission and shutdown behavior are implemented at the
//! desktop edge; this policy is the single gate that edge must consult.

use bevy::prelude::Resource;

/// Whether the operating system has granted microphone access.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicrophonePermission {
    #[default]
    Unknown,
    Granted,
    Denied,
}

/// Which local microphone activation policy is selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicrophoneMode {
    /// No activation mode has been selected; transmission stays closed.
    #[default]
    Unselected,
    /// Transmit whenever voice is enabled and the player is in a multiplayer session.
    OpenMicrophone,
    /// Transmit only while the configured push-to-talk key is held.
    PushToTalk,
}

/// Local capture and transmission gates.
///
/// A backend may open the input device only when `capture_permitted` is true. It must
/// discard queued frames whenever `transmission_permitted` becomes false and must stop
/// the device when capture is no longer permitted.
#[derive(Clone, Copy, Debug, Resource)]
pub struct VoiceInputPolicy {
    permission: MicrophonePermission,
    mode: MicrophoneMode,
    enabled: bool,
    session_active: bool,
    muted: bool,
    push_to_talk_held: bool,
}

impl Default for VoiceInputPolicy {
    fn default() -> Self {
        Self {
            permission: MicrophonePermission::Unknown,
            mode: MicrophoneMode::Unselected,
            enabled: false,
            session_active: false,
            muted: false,
            push_to_talk_held: false,
        }
    }
}

impl VoiceInputPolicy {
    /// Updates OS permission; revocation also releases the PTT latch.
    pub fn set_permission(&mut self, permission: MicrophonePermission) {
        self.permission = permission;
        if permission != MicrophonePermission::Granted {
            self.push_to_talk_held = false;
        }
    }

    /// Enables or disables voice. Disabling releases PTT immediately.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.push_to_talk_held = false;
        }
    }

    /// Tracks whether a joined multiplayer world currently owns this voice session.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
        if !active {
            self.push_to_talk_held = false;
        }
    }

    /// Selects open microphone or PTT. A mode change requires a fresh PTT press.
    pub fn set_mode(&mut self, mode: MicrophoneMode) {
        if self.mode != mode {
            self.push_to_talk_held = false;
        }
        self.mode = mode;
    }

    /// Local mute blocks transmission immediately and releases the PTT latch.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if muted {
            self.push_to_talk_held = false;
        }
    }

    /// Updates the physical PTT state supplied by the input adapter.
    pub fn set_push_to_talk_held(&mut self, held: bool) {
        self.push_to_talk_held = held
            && self.permission == MicrophonePermission::Granted
            && self.enabled
            && self.session_active
            && !self.muted;
    }

    /// Whether opening/keeping the local input device is permitted after a successful grant.
    #[must_use]
    pub fn capture_permitted(&self) -> bool {
        self.permission == MicrophonePermission::Granted && self.enabled && self.session_active
    }

    /// Whether captured frames may leave this client.
    #[must_use]
    pub fn transmission_permitted(&self) -> bool {
        self.capture_permitted()
            && !self.muted
            && match self.mode {
                MicrophoneMode::Unselected => false,
                MicrophoneMode::OpenMicrophone => true,
                MicrophoneMode::PushToTalk => self.push_to_talk_held,
            }
    }

    #[must_use]
    pub fn permission(&self) -> MicrophonePermission {
        self.permission
    }

    #[must_use]
    pub fn mode(&self) -> MicrophoneMode {
        self.mode
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn session_active(&self) -> bool {
        self.session_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> VoiceInputPolicy {
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(MicrophonePermission::Granted);
        policy.set_enabled(true);
        policy.set_session_active(true);
        policy
    }

    #[test]
    fn unselected_mode_never_transmits() {
        let policy = ready();
        assert_eq!(policy.mode(), MicrophoneMode::Unselected);
        assert!(policy.capture_permitted());
        assert!(!policy.transmission_permitted());
    }

    #[test]
    fn capture_stays_closed_until_permission_voice_and_session_are_ready() {
        let mut policy = VoiceInputPolicy::default();
        assert!(!policy.capture_permitted());
        policy.set_enabled(true);
        policy.set_session_active(true);
        assert!(!policy.capture_permitted());
        policy.set_permission(MicrophonePermission::Granted);
        assert!(policy.capture_permitted());
    }

    #[test]
    fn open_microphone_transmits_only_while_enabled_in_a_session() {
        let mut policy = ready();
        policy.set_mode(MicrophoneMode::OpenMicrophone);
        assert!(policy.transmission_permitted());
        policy.set_enabled(false);
        assert!(!policy.capture_permitted());
        assert!(!policy.transmission_permitted());
    }

    #[test]
    fn push_to_talk_requires_a_fresh_press_and_release() {
        let mut policy = ready();
        policy.set_mode(MicrophoneMode::PushToTalk);
        assert!(!policy.transmission_permitted());
        policy.set_push_to_talk_held(true);
        assert!(policy.transmission_permitted());
        policy.set_push_to_talk_held(false);
        assert!(!policy.transmission_permitted());
    }

    #[test]
    fn mute_disconnect_and_permission_revocation_stop_transmission_immediately() {
        let mut policy = ready();
        policy.set_mode(MicrophoneMode::PushToTalk);
        policy.set_push_to_talk_held(true);
        assert!(policy.transmission_permitted());

        policy.set_muted(true);
        assert!(!policy.transmission_permitted());
        policy.set_muted(false);
        assert!(
            !policy.transmission_permitted(),
            "mute must release the PTT latch"
        );

        policy.set_push_to_talk_held(true);
        policy.set_session_active(false);
        assert!(!policy.capture_permitted());
        assert!(!policy.transmission_permitted());

        policy.set_session_active(true);
        policy.set_push_to_talk_held(true);
        policy.set_permission(MicrophonePermission::Denied);
        assert!(!policy.capture_permitted());
        assert!(!policy.transmission_permitted());
    }

    #[test]
    fn changing_mode_or_disabling_voice_cannot_reuse_a_stale_ptt_press() {
        let mut policy = ready();
        policy.set_mode(MicrophoneMode::PushToTalk);
        policy.set_push_to_talk_held(true);
        policy.set_mode(MicrophoneMode::OpenMicrophone);
        policy.set_mode(MicrophoneMode::PushToTalk);
        assert!(!policy.transmission_permitted());
        policy.set_push_to_talk_held(true);
        policy.set_enabled(false);
        policy.set_enabled(true);
        assert!(!policy.transmission_permitted());
    }
}
