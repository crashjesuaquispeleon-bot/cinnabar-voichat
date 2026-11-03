use super::{
    inbox, invite, realm_membership, server_list, settings_options, settings_storage,
    settings_support, updates, LocalWorldAction, MenuScreen, MenuServerTab, ProfileTab,
    RendererChoice,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    MinecraftImport(crate::minecraft_import::Action),
    Update(updates::Action),
    OpenAccounts,
    AddAccount,
    SwitchAccount(usize),
    Inbox(inbox::Action),
    Navigate(MenuScreen),
    OpenExitDialog,
    ConfirmExit,
    DismissDialog,
    /// The quit dialog's header close control, independent of its Cancel button.
    CloseExitDialog,
    Reconnect,
    SelectServerTab(MenuServerTab),
    ServerList(server_list::ServerListAction),
    SelectProfileTab(ProfileTab),
    RefreshProfile,
    RefreshCatalog,
    StartSignIn,
    CancelSignIn,
    /// The sign-in header's close control, with feedback independent of the cancel button.
    CloseSignIn,
    OpenSignInLink,
    PlayAddServer,
    PlaySaved(usize),
    PlayFeatured(usize),
    PlayRealm(usize),
    /// Opens or advances Realm membership through an invite link or code.
    RealmMembership(realm_membership::Action),
    PlayFriend(usize),
    ToggleFavorite(usize),
    RemoveSavedDialog(usize),
    ConfirmRemoveSaved(usize),
    AddName,
    AddAddress,
    AddPort,
    EditSkinName,
    SearchFriends,
    SearchSettings,
    ClearSettingsSearch,
    /// Opens a matching category and clears its search query.
    SettingsSearchSection(u8),
    AddSave,
    AddSaveConnect,
    AddBack,
    /// Opens the native touch-control layout draft.
    EditTouchControls,
    SettingsScale(i8),
    SettingsScalePicker,
    SettingsFullscreen(bool),
    SettingsStorage(settings_storage::StorageAction),
    SettingsSupport(settings_support::SupportAction),
    /// Refreshes the local microphone catalog on the native device-owner thread.
    SettingsMicrophoneRefresh,
    /// Explicitly switches capture back to the operating-system default input.
    SettingsMicrophoneUseSystemDefault,
    /// Retries the OS permission probe after the player changes system privacy settings.
    SettingsMicrophoneRetryPermission,
    /// Selects a row from one exact presentation snapshot; stale revisions are rejected.
    SettingsMicrophoneSelect(u16, u64),
    SettingsOption(u16, i32),
    /// Applies one graphics preset without changing personal video preferences.
    SettingsVideoPreset(settings_options::video_presets::VideoPreset),
    SettingsDropdown(u16),
    SettingsLanguage(u16),
    SettingsKey(u16),
    SettingsResetKey(u16),
    SettingsResetBindings(bool),
    /// Presses a command macro's command text box.
    SettingsMacroCommand(usize),
    SettingsResetGroup(settings_options::SettingsGroup),
    SettingsConfirmResetBindings(bool),
    SettingsConfirmResetGroup(settings_options::SettingsGroup),
    SettingsResetChat,
    SettingsAdvancedGraphics,
    /// The JSON-UI Enhanced switch: Vanilla and Enhanced flip; a shader pack turns into Enhanced.
    ToggleRenderMode,
    /// The OreUI renderer and shader-pack pickers.
    SelectRenderer(RendererChoice),
    /// Opens the Shaders settings tab from Video.
    SettingsShaders,
    /// Re-lists shader packs and reloads the selected pack with its saved options.
    ReloadShaders,
    /// Opens the platform file picker to add shader-pack archives.
    ImportShaderPacks,
    RemoveShaderPackDialog(u16, u64),
    ConfirmRemoveShaderPack(u16, u64),
    PauseResume,
    PauseDisconnect,
    OpenDeathQuit,
    OpenDeathGameMenu,
    DeathExitWorld,
    ConfirmDeathQuit,
    PauseSettings,
    /// Load a saved server into the add/edit draft.
    EditSaved(usize),
    /// Pick a settings section by its selector index.
    SettingsSection(u8),
    Respawn,
    PlayLocalWorld(usize),
    /// A press on a local-world screen (create, edit, templates) or its modals.
    LocalWorld(LocalWorldAction),
    /// A press in Settings' Game section for the world being played.
    WorldSettings(crate::local_worlds::in_world::Action),
    SignOut,
    /// Show a featured server in the Servers tab's info panel.
    SelectFeatured(usize),
    /// Show a saved server's details on the Servers tab.
    SelectSaved(usize),
    /// Show a Realm's details on the Realms tab.
    SelectRealm(usize),
    /// Flip the info panel's description (0) or news (1) past "read more".
    ToggleReadMore(u8),
    /// The start screen's live-event button.
    OpenLiveEvent,
    GlobalResources(crate::global_resources::Action),
    /// A press on the Settings Mods tab.
    Mods(crate::mods::Action),
    DressingRoom(crate::dressing_room::Action),
    /// Answers the join's server trust prompt: "Trust and Join" (true) or "Don't Trust".
    ServerTrust(bool),
    /// A press on the pause screen's invite button or the invite screen.
    Invite(invite::Action),
    /// Answers the oldest Discord join request: Accept (true) or Decline.
    JoinRequest(bool),
    /// A press on the Shader Pack Settings screen or its Shaders-tab button.
    ShaderOptions(crate::shader_options::Action),
}
