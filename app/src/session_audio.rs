//! Borrows session identity for the presentation audio lane.
use bevy::prelude::*;
use client_audio::audio::{
    voice_controller::VoiceCaptureController, voice_input::VoiceInputPolicy,
};
use client_audio::audio_ingress::SequencedAudioEvent;
use client_audio::session_audio::{SessionAudio, SessionAudioCatalog};
use semantic_input::Action;

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
    mut voice_session: Local<Option<(u64, u64)>>,
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
