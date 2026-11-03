//! Borrows session identity for the presentation audio lane.
use bevy::prelude::*;
use client_audio::audio::{
    voice_controller::VoiceCaptureController,
    voice_input::{MicrophoneMode, VoiceInputPolicy},
};
use client_audio::audio_ingress::SequencedAudioEvent;
use client_audio::session_audio::{SessionAudio, SessionAudioCatalog};
use semantic_input::Action;

/// Copies saved, local voice preferences into the capture policy.
fn apply_voice_chat_settings(policy: &mut VoiceInputPolicy, settings: &ui::VoiceChatSettings) {
    policy.set_enabled(settings.enabled);
    policy.set_mode(match settings.activation_mode {
        ui::VoiceActivationMode::OpenMicrophone => MicrophoneMode::OpenMicrophone,
        ui::VoiceActivationMode::PushToTalk => MicrophoneMode::PushToTalk,
    });
}

/// Applies the saved Sound-panel mute only when that preference changes.
/// This allows the in-game quick-mute action to override it between settings edits.
fn apply_saved_voice_mute(
    policy: &mut VoiceInputPolicy,
    saved_muted: bool,
    last_saved_mute: &mut Option<bool>,
) {
    if *last_saved_mute != Some(saved_muted) {
        policy.set_muted(saved_muted);
        *last_saved_mute = Some(saved_muted);
    }
}

/// Toggles the current local mute state after a one-shot semantic input press.
fn toggle_quick_voice_mute(policy: &mut VoiceInputPolicy) {
    policy.set_muted(!policy.muted());
}

/// Borrows current owner facts and forwards them at the existing system boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drain_sequenced_audio_into_session(
    messages: MessageReader<SequencedAudioEvent>,
    clock: Res<client_presentation::environment::WorldClock>,
    client_world: Res<crate::runtime::world::ClientWorld>,
    catalog: Res<SessionAudioCatalog>,
    #[cfg(feature = "acceptance")] mut wire_messages: MessageReader<SequencedAudioEvent>,
    #[cfg(feature = "acceptance")] mut wire: Option<ResMut<acceptance::audio_wire::WireEvidence>>,
    session: ResMut<SessionAudio>,
    mut voice_policy: ResMut<VoiceInputPolicy>,
    mut voice_controller: ResMut<VoiceCaptureController>,
    input: Res<client_state::SemanticInputSnapshot>,
    settings: Res<client_state::RuntimeSettings>,
    mut voice_session: Local<Option<(u64, u64)>>,
    mut last_saved_voice_mute: Local<Option<bool>>,
) {
    // Bind microphone lifetime to the exact local WorldStream lifetime and the
    // session generation. A transfer/reconnect can replace the stream without
    // leaving a frame in which the old microphone queue remains usable.
    let current_voice_session = client_world.stream.as_ref().map(|stream| {
        (
            clock.session_generation(),
            stream.authority().actor_session_id(),
        )
    });
    if current_voice_session != *voice_session {
        voice_controller.reset_for_session_change();
        voice_policy.set_session_active(current_voice_session.is_some());
        *voice_session = current_voice_session;
    }
    // Saved Sound-panel preferences configure the local voice policy. Voice stays
    // disabled by default; capture still requires OS permission and a live game session.
    let voice_settings = &settings.user_settings_update().1.voice_chat;
    apply_voice_chat_settings(&mut voice_policy, voice_settings);
    apply_saved_voice_mute(
        &mut voice_policy,
        voice_settings.muted,
        &mut last_saved_voice_mute,
    );
    // Quick mute is a remappable one-shot action, not frame-by-frame key polling.
    if input.phase(Action::ToggleVoiceMute).pressed {
        toggle_quick_voice_mute(&mut voice_policy);
    }
    // PTT is sourced from the finalized semantic action, never raw keyboard polling.
    // Missing/stale snapshots report the neutral released phase.
    voice_policy.set_push_to_talk_held(input.phase(Action::PushToTalk).held);
    #[cfg(feature = "acceptance")]
    if let Some(wire) = wire.as_mut() {
        let stream_id = client_world
            .stream
            .as_ref()
            .map(|stream| stream.authority().actor_session_id());
        wire.bind(stream_id);
        if let Some(stream_id) = stream_id {
            for event in wire_messages.read() {
                if event.origin_stream_session_id == stream_id {
                    wire.emit(stream_id, event);
                }
            }
        } else {
            wire_messages.clear();
        }
    }
    client_audio::session_audio::drain_sequenced_audio_into_session(
        messages,
        client_audio::observations::SessionObservation(clock.session_generation()),
        client_audio::observations::WorldObservation {
            stream: client_world.stream.as_ref(),
        },
        catalog,
        session,
    );
}

#[cfg(all(test, feature = "acceptance"))]
#[path = "session_audio/wire_evidence_tests.rs"]
mod wire_evidence_tests;

#[cfg(test)]
mod voice_settings_tests {
    use super::*;

    #[test]
    fn voice_chat_settings_are_applied_with_closed_default_and_open_mic_choice() {
        let mut policy = VoiceInputPolicy::default();
        let defaults = ui::VoiceChatSettings::default();
        apply_voice_chat_settings(&mut policy, &defaults);

        assert!(!policy.enabled());
        assert_eq!(policy.mode(), MicrophoneMode::PushToTalk);
        assert!(!policy.capture_permitted());

        let open_mic = ui::VoiceChatSettings {
            enabled: true,
            muted: false,
            activation_mode: ui::VoiceActivationMode::OpenMicrophone,
        };
        apply_voice_chat_settings(&mut policy, &open_mic);
        policy.set_permission(client_audio::audio::voice_input::MicrophonePermission::Granted);
        policy.set_session_active(true);

        assert!(policy.enabled());
        assert_eq!(policy.mode(), MicrophoneMode::OpenMicrophone);
        assert!(policy.transmission_permitted());
    }

    #[test]
    fn local_mute_stops_open_microphone_without_disabling_voice() {
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(client_audio::audio::voice_input::MicrophonePermission::Granted);
        policy.set_session_active(true);
        let mut last_saved_mute = None;
        let unmuted = ui::VoiceChatSettings {
            enabled: true,
            muted: false,
            activation_mode: ui::VoiceActivationMode::OpenMicrophone,
        };
        apply_voice_chat_settings(&mut policy, &unmuted);
        apply_saved_voice_mute(&mut policy, unmuted.muted, &mut last_saved_mute);
        assert!(policy.transmission_permitted());

        apply_saved_voice_mute(&mut policy, true, &mut last_saved_mute);
        assert!(policy.enabled());
        assert!(!policy.transmission_permitted());

        apply_saved_voice_mute(&mut policy, false, &mut last_saved_mute);
        assert!(policy.transmission_permitted());
    }

    #[test]
    fn quick_mute_toggles_and_saved_mute_changes_reset_the_runtime_override() {
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(client_audio::audio::voice_input::MicrophonePermission::Granted);
        policy.set_session_active(true);
        let mut last_saved_mute = None;
        let settings = ui::VoiceChatSettings {
            enabled: true,
            muted: false,
            activation_mode: ui::VoiceActivationMode::OpenMicrophone,
        };
        apply_voice_chat_settings(&mut policy, &settings);
        apply_saved_voice_mute(&mut policy, settings.muted, &mut last_saved_mute);
        assert!(policy.transmission_permitted());

        toggle_quick_voice_mute(&mut policy);
        assert!(policy.muted());
        assert!(!policy.transmission_permitted());

        // Unchanged persisted settings must not erase the in-game override each frame.
        apply_saved_voice_mute(&mut policy, false, &mut last_saved_mute);
        assert!(policy.muted());

        // The router reports a fresh pressed edge once, not on repeated held frames.
        toggle_quick_voice_mute(&mut policy);
        assert!(!policy.muted());
        assert!(policy.transmission_permitted());

        // A persisted Sound-panel edit becomes the new baseline.
        toggle_quick_voice_mute(&mut policy);
        assert!(policy.muted());
        apply_saved_voice_mute(&mut policy, true, &mut last_saved_mute);
        assert!(policy.muted());
        toggle_quick_voice_mute(&mut policy);
        assert!(!policy.muted());
        assert!(policy.transmission_permitted());
    }

    #[test]
    fn quick_mute_does_not_restore_a_ptt_press_that_started_before_muting() {
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(client_audio::audio::voice_input::MicrophonePermission::Granted);
        policy.set_session_active(true);
        let settings = ui::VoiceChatSettings {
            enabled: true,
            muted: false,
            activation_mode: ui::VoiceActivationMode::PushToTalk,
        };
        apply_voice_chat_settings(&mut policy, &settings);
        let mut last_saved_mute = None;
        apply_saved_voice_mute(&mut policy, settings.muted, &mut last_saved_mute);
        policy.set_push_to_talk_held(false);
        policy.set_push_to_talk_held(true);
        assert!(policy.transmission_permitted());

        toggle_quick_voice_mute(&mut policy);
        policy.set_push_to_talk_held(true);
        assert!(!policy.transmission_permitted());
        toggle_quick_voice_mute(&mut policy);
        policy.set_push_to_talk_held(true);
        assert!(!policy.transmission_permitted());

        // A physical release and a new PTT press are required after unmuting.
        policy.set_push_to_talk_held(false);
        policy.set_push_to_talk_held(true);
        assert!(policy.transmission_permitted());
    }

    #[test]
    fn disabling_voice_and_switching_to_ptt_releases_transmission() {
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(client_audio::audio::voice_input::MicrophonePermission::Granted);
        policy.set_session_active(true);
        apply_voice_chat_settings(
            &mut policy,
            &ui::VoiceChatSettings {
                enabled: true,
                muted: false,
                activation_mode: ui::VoiceActivationMode::OpenMicrophone,
            },
        );
        assert!(policy.transmission_permitted());

        apply_voice_chat_settings(
            &mut policy,
            &ui::VoiceChatSettings {
                enabled: true,
                muted: false,
                activation_mode: ui::VoiceActivationMode::PushToTalk,
            },
        );
        assert!(!policy.transmission_permitted());
        policy.set_push_to_talk_held(false);
        policy.set_push_to_talk_held(true);
        assert!(policy.transmission_permitted());

        apply_voice_chat_settings(&mut policy, &ui::VoiceChatSettings::default());
        assert!(!policy.transmission_permitted());
    }
}
