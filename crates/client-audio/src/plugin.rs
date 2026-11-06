use crate::audio::{AudioSettings, LocalBlockCue, UiSoundCue};
use bevy::prelude::IntoScheduleConfigs;

/// Installs audio's settings and local cue queues without changing frame ordering.
pub struct ClientAudioPlugin;
impl bevy::prelude::Plugin for ClientAudioPlugin {
    /// Initializes retained audio state and local cue queues.
    fn build(&self, app: &mut bevy::prelude::App) {
        app.init_resource::<crate::session_audio::SessionAudio>()
            .init_resource::<crate::named_audio::NamedAudio>()
            .init_resource::<crate::audio::AudioEngine>()
            .init_resource::<AudioSettings>()
            .init_resource::<crate::audio::voice_input::VoiceInputPolicy>()
            .init_resource::<crate::audio::voice_controller::VoiceCaptureController>()
            .init_resource::<crate::audio::voice_loopback::VoiceLoopbackAttachment>()
            .init_resource::<crate::audio::voice_remote::VoiceRemotePlayback>()
            .init_resource::<crate::audio::voice_remote::VoiceRemoteAttachment>()
            .add_systems(
                bevy::prelude::Update,
                (
                    crate::audio::voice_controller::sync_voice_capture_with_policy,
                    crate::audio::voice_loopback::attach_voice_loopback_to_named_audio
                        .after(crate::audio::voice_controller::sync_voice_capture_with_policy),
                    crate::audio::voice_remote::attach_remote_voice_to_named_audio
                        .after(crate::audio::voice_controller::sync_voice_capture_with_policy),
                ),
            )
            .add_message::<UiSoundCue>()
            .add_message::<LocalBlockCue>()
            .add_message::<crate::audio::predicted::LocalEatingCue>()
            .add_message::<crate::audio::predicted::LocalAttackCue>();
    }
}
