//! Drives account, world and settings services from the menu.
use crate::runtime::{network::resource_packs::PackApplication, world::ClientWorld};
use bevy::prelude::*;
use client_audio::audio::{
    voice_capture::{MicrophoneSelection, MicrophoneWorkerEvent},
    voice_controller::VoiceCaptureController,
    voice_input::{MicrophoneMode, MicrophonePermission, VoiceInputPolicy},
};
use client_runtime::{blob_cache::ClientBlobCacheOwner, network::NetworkHandle};
use client_shell::{
    menu::{LauncherCoreSlot, MenuRuntime, launcher_account},
    player_skin::LocalPlayerSkin,
    ui_resources::UiRuntimeResource,
};
use launcher::menu::view::{MicrophonePermissionStatus, MicrophoneSelectionRequest};
#[derive(bevy::ecs::system::SystemParam)]
struct MenuVoiceSettings<'w> {
    audio_settings: Option<ResMut<'w, client_audio::audio::AudioSettings>>,
    voice_controller: Option<ResMut<'w, VoiceCaptureController>>,
    voice_policy: Option<ResMut<'w, VoiceInputPolicy>>,
}

/// Drives the launcher's own services: catalog, saves, settings, the account core and local worlds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_menu_services(
    mut commands: Commands,
    mut menu: ResMut<MenuRuntime>,
    keys: Res<ButtonInput<KeyCode>>,
    client_blob_cache: Res<ClientBlobCacheOwner>,
    player_runtime: Res<client_state::PlayerRuntime>,
    mut client_world: ResMut<ClientWorld>,
    mut runtime: ResMut<UiRuntimeResource>,
    launcher: Option<ResMut<LauncherCoreSlot>>,
    launcher_account: Option<ResMut<launcher_account::LauncherAccount>>,
    mut local_worlds: Option<ResMut<client_shell::local_worlds::LocalWorlds>>,
    voice: MenuVoiceSettings,
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
    sync_audio_settings(&menu, voice.audio_settings);
    if let Some(support) = antialiasing {
        menu.sync_anti_aliasing_support(support.0);
    }
    menu.sync_user_settings(settings);
    let voice_session_available = client_world.stream.is_some()
        && network
            .as_ref()
            .and_then(|network| network.core_socket_dir())
            .map(std::path::PathBuf::from)
            .is_some_and(|directory| bridge::voice_endpoint_path(&directory).exists());
    drive_microphone_settings(
        &mut menu,
        voice.voice_controller,
        voice.voice_policy,
        voice_session_available,
        keys.pressed(KeyCode::KeyV),
    );
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

/// Applies saved Sound-panel values to the real microphone gates.
///
/// Voice IPC availability only decides whether this client may open its local capture
/// device. The signed broker admission remains the sole authority for room membership.
fn apply_voice_preferences(
    options: &launcher::menu::settings_options::SettingsOptions,
    controller: Option<&VoiceCaptureController>,
    policy: &mut VoiceInputPolicy,
    voice_session_available: bool,
    push_to_talk_held: bool,
) {
    use launcher::menu::settings_options::SettingId;

    policy.set_enabled(options.value(SettingId::VoiceChatEnabled) != 0);
    policy.set_mode(match options.value(SettingId::VoiceActivationMode) {
        0 => MicrophoneMode::OpenMicrophone,
        _ => MicrophoneMode::PushToTalk,
    });
    policy.set_muted(options.value(SettingId::VoiceChatMuted) != 0);
    policy.set_session_active(voice_session_available);
    policy.set_push_to_talk_held(push_to_talk_held);

    if let Some(controller) = controller {
        controller.set_input_sensitivity_percent(
            options
                .value(SettingId::VoiceInputSensitivity)
                .clamp(0, 200) as u8,
        );
    }
}

/// Routes presentation-only microphone actions and worker events without blocking the menu.
fn drive_microphone_settings(
    menu: &mut MenuRuntime,
    mut controller: Option<ResMut<VoiceCaptureController>>,
    mut voice_policy: Option<ResMut<VoiceInputPolicy>>,
    voice_session_available: bool,
    push_to_talk_held: bool,
) {
    let (options, _) = menu.settings_snapshot();
    if let Some(policy) = voice_policy.as_deref_mut() {
        apply_voice_preferences(
            &options,
            controller.as_deref(),
            policy,
            voice_session_available,
            push_to_talk_held,
        );
    } else if let Some(controller) = controller.as_deref() {
        controller.set_input_sensitivity_percent(
            options
                .value(launcher::menu::settings_options::SettingId::VoiceInputSensitivity)
                .clamp(0, 200) as u8,
        );
    }

    if menu.take_microphone_permission_retry_request() {
        if let Some(controller) = controller.as_deref_mut() {
            controller.retry_permission_probe();
        }
        if let Some(policy) = voice_policy.as_deref_mut() {
            // A fresh open attempt is allowed only if voice remains enabled in a live session.
            policy.set_permission(MicrophonePermission::Unknown);
            menu.set_microphone_message(
                "Permission check restarted. Enable Voice Chat in an active multiplayer session to prompt the OS again.",
            );
        } else {
            menu.set_microphone_message(
                "Microphone permission state is unavailable on this client.",
            );
        }
    }

    let permission =
        voice_policy.map_or(MicrophonePermission::Unknown, |policy| policy.permission());
    menu.set_microphone_permission_status(match permission {
        MicrophonePermission::Unknown => MicrophonePermissionStatus::Unknown,
        MicrophonePermission::Granted => MicrophonePermissionStatus::Granted,
        MicrophonePermission::Denied => MicrophonePermissionStatus::Denied,
    });

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
            MicrophoneWorkerEvent::DeviceUnavailable { .. } => {
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
            MicrophoneSelectionRequest::SystemDefault => MicrophoneSelection::SystemDefault,
            MicrophoneSelectionRequest::Device(id) => MicrophoneSelection::Device(id),
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

#[cfg(test)]
mod voice_settings_tests {
    use super::*;
    use launcher::menu::settings_options::{SettingId, SettingsOptions};

    fn enabled_options(mode: i32) -> SettingsOptions {
        let mut options = SettingsOptions::default();
        assert!(options.set(SettingId::VoiceChatEnabled.index(), 1));
        if mode != 0 {
            assert!(options.set(SettingId::VoiceActivationMode.index(), mode));
        }
        options
    }

    #[test]
    fn open_microphone_uses_saved_enablement_only_with_a_live_voice_endpoint() {
        let options = enabled_options(0);
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(MicrophonePermission::Granted);

        apply_voice_preferences(&options, None, &mut policy, false, false);
        assert!(policy.enabled());
        assert!(!policy.session_active());
        assert!(!policy.transmission_permitted());

        apply_voice_preferences(&options, None, &mut policy, true, false);
        assert_eq!(policy.mode(), MicrophoneMode::OpenMicrophone);
        assert!(policy.capture_permitted());
        assert!(policy.transmission_permitted());
    }

    #[test]
    fn push_to_talk_uses_v_and_mute_overrides_the_held_key() {
        let mut options = enabled_options(1);
        let mut policy = VoiceInputPolicy::default();
        policy.set_permission(MicrophonePermission::Granted);

        apply_voice_preferences(&options, None, &mut policy, true, false);
        assert_eq!(policy.mode(), MicrophoneMode::PushToTalk);
        assert!(policy.capture_permitted());
        assert!(!policy.transmission_permitted());

        apply_voice_preferences(&options, None, &mut policy, true, true);
        assert!(policy.transmission_permitted());

        assert!(options.set(SettingId::VoiceChatMuted.index(), 1));
        apply_voice_preferences(&options, None, &mut policy, true, true);
        assert!(policy.muted());
        assert!(!policy.transmission_permitted());
    }
}
