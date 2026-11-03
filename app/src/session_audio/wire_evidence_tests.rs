use acceptance::audio_wire::WireEvidence;
use client_audio::audio_ingress::SequencedAudioEvent;
use client_audio::session_audio::SessionAudio;
use protocol::{AudioEvent, PlayAudioEvent};

fn event(session: u64, sequence: u64) -> SequencedAudioEvent {
    SequencedAudioEvent {
        origin_stream_session_id: session,
        dimension: 0,
        dimension_epoch: 0,
        client_generated: false,
        actor_synchronization: None,
        sequence,
        event: AudioEvent::Play(PlayAudioEvent {
            name: std::sync::Arc::from("random.orb"),
            position: [0, 64, 0],
            volume: 1.0,
            pitch: 1.0,
            loop_count: 0,
            server_sound_handle: None,
        }),
    }
}

fn stream() -> chunk_pipeline::WorldStream {
    chunk_pipeline::WorldStream::new(chunk_pipeline::WorldBootstrap {
        dimension: 0,
        local_player_runtime_id: 1,
        local_player_unique_id: 1,
        player_position: [0.0, 64.0, 0.0],
        world_spawn_position: [0, 64, 0],
        air_network_id: protocol_ids::SEQUENTIAL_AIR_NETWORK_ID,
        block_network_ids_are_hashes: false,
    })
}

fn forwarded(stream: &mut chunk_pipeline::WorldStream, sequence: u64) -> Vec<SequencedAudioEvent> {
    stream
        .submit(
            sequence,
            protocol::WorldEvent::Audio(event(stream.authority().actor_session_id(), sequence).event),
        )
        .unwrap();
    let mut events = Vec::new();
    client_audio::audio_ingress::drain_committed_audio(stream, |event| events.push(event));
    events
}

fn app(stream: chunk_pipeline::WorldStream, enabled: bool) -> bevy::prelude::App {
    let mut app = bevy::prelude::App::new();
    app.add_message::<SequencedAudioEvent>()
        .init_resource::<client_presentation::environment::WorldClock>()
        .init_resource::<client_audio::audio::voice_input::VoiceInputPolicy>()
        .init_resource::<client_audio::audio::voice_controller::VoiceCaptureController>()
        .init_resource::<client_state::SemanticInputSnapshot>()
        .insert_resource(crate::runtime::world::ClientWorld {
            stream: Some(stream),
            ..crate::runtime::world::ClientWorld::default()
        })
        .insert_resource(client_audio::session_audio::SessionAudioCatalog(None))
        .insert_resource(SessionAudio::default())
        .insert_resource(WireEvidence::new(enabled))
        .add_systems(
            bevy::prelude::Update,
            crate::session_audio::drain_sequenced_audio_into_session,
        );
    app
}

fn write(app: &mut bevy::prelude::App, events: impl IntoIterator<Item = SequencedAudioEvent>) {
    let mut messages = app
        .world_mut()
        .resource_mut::<bevy::ecs::message::Messages<SequencedAudioEvent>>();
    for event in events {
        messages.write(event);
    }
}

#[test]
fn production_forwarding_rejects_old_buffered_stream_after_fifo_restart() {
    let mut old = stream();
    let old_id = old.authority().actor_session_id();
    let old_events = forwarded(&mut old, 1);
    assert_eq!(old_events[0].origin_stream_session_id, old_id);
    let mut app = app(old, true);
    write(&mut app, old_events);
    app.update();
    assert_eq!(
        app.world()
            .resource::<WireEvidence>()
            .observed_sequences()
            .len(),
        1
    );
    let old_buffered = {
        let mut world = app
            .world_mut()
            .resource_mut::<crate::runtime::world::ClientWorld>();
        forwarded(world.stream.as_mut().unwrap(), 2)
    };
    let mut replacement = stream();
    let new_id = replacement.authority().actor_session_id();
    assert_ne!(old_id, new_id);
    let new_events = forwarded(&mut replacement, 1);
    app.world_mut()
        .resource_mut::<crate::runtime::world::ClientWorld>()
        .stream = Some(replacement);
    write(&mut app, old_buffered.into_iter().chain(new_events));
    app.update();
    let audio = app.world().resource::<SessionAudio>();
    let rows = app.world().resource::<WireEvidence>().observed_sequences();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, new_id);
    assert_eq!(rows[0].1, 1);
    assert_eq!(
        audio.catalog_unavailable_total(),
        2,
        "replaced-stream events cannot reach catalog resolution"
    );
    assert_eq!(audio.admitted_total(), 0);
}

#[test]
fn production_evidence_precedes_missing_catalog_without_changing_resolution() {
    let mut stream = stream();
    let session = stream.authority().actor_session_id();
    let events = forwarded(&mut stream, 1);
    let mut app = app(stream, true);
    write(&mut app, events);
    app.update();
    let audio = app.world().resource::<SessionAudio>();
    let rows = app.world().resource::<WireEvidence>().observed_sequences();
    assert_eq!(rows, vec![(session, 1)]);
    assert_eq!(audio.catalog_unavailable_total(), 1);
    assert_eq!(audio.admitted_total(), 0);
    assert_eq!(audio.unknown_definition_total(), 0);
}
