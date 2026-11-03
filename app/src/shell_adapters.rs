//! Drives account, world and settings services from the menu.
use crate::runtime::{network::resource_packs::PackApplication, world::ClientWorld};
use bevy::prelude::*;
use client_audio::audio::{
    voice_capture::{MicrophoneSelection, MicrophoneWorkerEvent},
    voice_controller::VoiceCaptureController,
};
use client_runtime::{blob_cache::ClientBlobCacheOwner, network::NetworkHandle};
use client_shell::{
    menu::{LauncherCoreSlot, MenuRuntime, launcher_account},
    player_skin::LocalPlayerSkin,
    ui_resources::UiRuntimeResource,
};
use launcher::menu::view::MicrophoneSelectionRequest;
/// Drives the launcher's own services: catalog, saves, settings, the account core and local worlds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_menu_services(
    mut commands: Commands,
    mut menu: ResMut<MenuRuntime>,
    client_blob_cache: Res<ClientBlobCacheOwner>,
    player_runtime: Res<client_state::PlayerRuntime>,
    mut client_world: ResMut<ClientWorld>,
    mut runtime: ResMut<UiRuntimeResource>,
    launcher: Option<ResMut<LauncherCoreSlot>>,
    launcher_account: Option<ResMut<launcher_account::LauncherAccount>>,
    mut local_worlds: Option<ResMut<client_shell::local_worlds::LocalWorlds>>,
    audio_settings: Option<ResMut<client_audio::audio::AudioSettings>>,
    mut voice_controller:
        Option<ResMut<VoiceCaptureController>>,
    settings: Option<ResMut<client_state::RuntimeSettings>>,
    antialiasing: Option<Res<client_presentation::camera::antialiasing::CameraAntiAliasingSupport>>,
    mut local_skin: Option<ResMut<LocalPlayerSkin>>,
    network: Option<Res<NetworkHandle<PackApplication>>>,
) {
    #[cfg(feature = "developer-control")]
    if menu.fixture_active() {
        return;
    }
    let send = network
        .as_ref()
        .map(|network| move |generation, packet| network.send_settings_packet(generation, packet));
    menu.poll_dressing_room(
        local_skin.as_deref_mut(),
        client_world.stream.as_mut(),
        send.as_ref().map(|send| send as &dyn Fn(_, _) -> _),
        runtime.session_id(),
    );
    menu.poll_catalog(launcher_account.is_some());
    menu.poll_saves();
    menu.poll_accounts();
    sync_audio_settings(&menu, audio_settings);
    if let Some(support) = antialiasing {
        menu.sync_anti_aliasing_support(support.0);
    }
    menu.sync_user_settings(settings);
    drive_microphone_settings(&mut menu, voice_controller);
    menu.sync_language(&mut runtime);
    if let Some(account) = launcher_account.as_deref() {
        client_shell::menu::sync_cinnabar_players(account, &mut runtime.0);
    }
    // This reusable core permits caching; each Connect narrows that capability using its
    // session cache mode before the core advertises it upstream.
    menu.drive_account_services(
        &mut commands,
        client_blob_cache.enables_upstream_client_cache(),
        &player_runtime,
        client_world.stream.is_some(),
        launcher,
        launcher_account,
        local_worlds.as_deref_mut(),
    );
}

/// Routes presentation-only microphone actions and worker events without blocking the menu.
fn drive_microphone_settings(
    menu: &mut MenuRuntime,
    controller: Option<ResMut<VoiceCaptureController>>,
) {
    let Some(mut controller) = controller else {
        if menu.take_microphone_refresh_request() {
            menu.set_microphone_message("Microphone control is unavailable on this client.");
        }
        if menu.take_microphone_selection_request().is_some() {
            menu.set_microphone_message("Microphone control is unavailable on this client.");
        }
        return;
    };

    // A stopped capture worker invalidates every UI token from its prior catalog.
    if !controller.supports_input_device_selection() && menu.has_microphone_devices() {
        menu.clear_microphone_devices(
            "Enable Voice Chat in an active multiplayer session to refresh microphone devices.",
        );
    }

    // Worker events carry only presentation tokens. A new snapshot invalidates the old list.
    for event in controller.poll_microphone_events() {
        match event {
            MicrophoneWorkerEvent::Devices(devices) => {
                menu.publish_microphone_devices(
                    devices
                        .into_iter()
                        .map(|device| launcher::menu::view::MicrophoneDeviceView {
                            id: device.id,
                            name: device.name,
                            is_default: device.is_default,
                        })
                        .collect(),
                );
            }
            MicrophoneWorkerEvent::Selected { device_id } => {
                menu.mark_microphone_selected(device_id);
            }
            MicrophoneWorkerEvent::SelectionRejected(error) => {
                menu.clear_microphone_devices(format!(
                    "The microphone selection was rejected: {error:?}. Refresh the list and try again."
                ));
            }
            MicrophoneWorkerEvent::DeviceUnavailable {
                ..
            } => {
                menu.clear_microphone_devices(
                    "The active microphone disconnected. Refresh the list to choose another input.",
                );
            }
            MicrophoneWorkerEvent::BackendError(error) => {
                menu.clear_microphone_devices(format!(
                    "Microphone device error: {error}. Refresh the list before choosing another input."
                ));
            }
            MicrophoneWorkerEvent::Stopped => {
                menu.clear_microphone_devices(
                    "The microphone worker stopped. Enable Voice Chat in a multiplayer session to scan again.",
                );
            }
        }
    }

    if menu.take_microphone_refresh_request() {
        match controller.refresh_microphone_devices() {
            Ok(()) => menu.set_microphone_message("Refreshing microphone input devices…"),
            Err(error) => menu.clear_microphone_devices(format!(
                "Could not refresh microphones: {error}. Enable Voice Chat in an active multiplayer session first."
            )),
        }
    }

    if let Some(request) = menu.take_microphone_selection_request() {
        let selection = match request {
            MicrophoneSelectionRequest::SystemDefault => {
                MicrophoneSelection::SystemDefault
            }
            MicrophoneSelectionRequest::Device(id) => {
                MicrophoneSelection::Device(id)
            }
        };
        if let Err(error) = controller.select_microphone_device(selection) {
            menu.clear_microphone_devices(format!(
                "Could not switch microphone: {error}. Refresh the list before trying again."
            ));
        }
    }
}

use client_audio::audio::AudioSettings;
/// Applies the saved sound values to the live mixer.
pub(crate) fn sync_audio_settings(menu: &MenuRuntime, settings: Option<ResMut<AudioSettings>>) {
    let Some(mut settings) = settings else {
        return;
    };
    let (options, _) = menu.settings_snapshot();
    for option in launcher::menu::settings_options::volume_settings() {
        if let Some(category) = option.volume {
            let volume = options.value(option.id) as f32 / 100.0;
            if settings.volume(category) != volume {
                settings.set(category, volume);
            }
        }
    }
}

/// Publishes the end-of-frame session observations: presence for the Discord driver and game
/// state for crash reports.
pub(crate) fn configure_observations(app: &mut App) {
    app.init_resource::<client_state::PresenceObservation>()
        .add_systems(
            Last,
            (
                publish_presence_observation.before(client_state::ShellFrameSet::Presence),
                publish_crash_state,
            ),
        );
}

/// Samples session presence at the same end-of-frame boundary as the Discord driver.
pub(crate) fn publish_presence_observation(
    world: Res<ClientWorld>,
    fatal_error: Res<crate::runtime::shutdown::FatalError>,
    session: Res<crate::session::SessionController>,
    mut observation: ResMut<client_state::PresenceObservation>,
) {
    observation.has_world = world.stream.is_some();
    observation.failed = fatal_error.0.is_some();
    observation.players = world
        .stream
        .as_ref()
        .map(|stream| u32::try_from(stream.authority().player_count()).unwrap_or(u32::MAX));
    if observation.target.as_ref() != session.presence_target() {
        observation.target = session.presence_target().cloned();
    }
}

/// Pushes the session phase, destination kind, dimension and server pack count to crash
/// reports, only when one of them changed.
pub(crate) fn publish_crash_state(
    world: Res<ClientWorld>,
    session: Res<crate::session::SessionController>,
    packs: Option<Res<client_runtime::pack_admission::ResourcePackAdmissionState>>,
    mut last: Local<Option<launcher_host::lifecycle::crash::state::GameState>>,
) {
    use launcher_host::lifecycle::crash::state::{GameState, SessionPhase};
    let stream = world.stream.as_ref();
    let phase = if stream.is_some() {
        SessionPhase::InWorld
    } else if session.status().connecting {
        SessionPhase::Joining
    } else {
        SessionPhase::Menus
    };
    let state = GameState {
        phase,
        server: (phase != SessionPhase::Menus)
            .then(|| session.presence_target().map(server_kind))
            .flatten(),
        dimension: stream.map(|stream| stream.current_dimension()),
        server_packs: packs.map_or(0, |packs| packs.pack_count()),
    };
    if last.as_ref() != Some(&state) {
        *last = Some(state);
        launcher_host::lifecycle::crash::state::publish(state);
    }
}

/// The kind of a join destination, without its address.
fn server_kind(
    target: &rich_presence::Target,
) -> launcher_host::lifecycle::crash::state::ServerKind {
    use launcher_host::lifecycle::crash::state::ServerKind;
    use rich_presence::Destination;
    match target.destination {
        Destination::Server(_) if target.badge.is_some() => ServerKind::FeaturedServer,
        Destination::Server(_) => ServerKind::Server,
        Destination::Realm => ServerKind::Realm,
        Destination::FriendWorld => ServerKind::FriendWorld,
        Destination::Experience => ServerKind::Experience,
        Destination::LocalWorld(_) => ServerKind::LocalWorld,
    }
}
