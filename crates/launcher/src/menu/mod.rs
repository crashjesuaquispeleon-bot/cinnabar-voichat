//! Launcher actions and immutable menu views shared with presentation.

use json_ui::vanilla::Reference;
mod action;
pub mod auth;
pub mod death;
pub mod dialogs;
pub mod disconnect;
pub mod flow;
pub mod inbox;
pub mod invite;
pub mod join_progress;
pub mod join_requests;
mod list;
pub mod profile;
pub mod profile_achievements;
pub mod realm_membership;
pub mod server_list;
pub mod settings_options;
pub mod settings_sections;
pub mod settings_storage;
pub mod settings_support;
pub mod shader_pack_status;
pub mod sign_in;
mod text_fields;
pub mod updates;
pub mod view;
pub mod worlds_tab;

pub use action::MenuAction;
pub use list::MenuList;
pub use profile::{
    ProfileTab, profile_banner_index, profile_count_display, profile_minutes_display,
};
pub use view::{
    ButtonArt, CatalogFile, CatalogFriend, CatalogServer, EXPERIENCE_ADDRESS_PREFIX,
    FRIEND_ADDRESS_PREFIX, InboxItem, JoinKind, JoinProgress, JoinStage, LiveEventCard,
    LocalWorldCard, MenuCaret, MenuFeeds, MenuFriendCard, MenuGameCard, MenuHome, MenuProfile,
    MenuRealmCard, MenuServerCard, MenuView, PingInfo, SavedServer, ServerDetails,
    ServerTrustPrompt, pingable,
};
pub use worlds_tab::{LocalWorldAction, civil_date, file_size};

/// Declares menu routes and their exhaustive traversal from one source.
macro_rules! menu_screens {
    ($($(#[$attribute:meta])* $name:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum MenuScreen { $($(#[$attribute])* $name),+ }
        impl MenuScreen {
            /// All declared routes, including routes added in future changes.
            pub const ALL: &[Self] = &[$(Self::$name),+];
            /// Screens entered only from an active play session.
            pub fn requires_session(self) -> bool {
                match self {
                    Self::Pause | Self::Death | Self::Invite => true,
                    Self::Home | Self::Play | Self::Social | Self::Servers | Self::Profile | Self::DressingRoom | Self::Settings | Self::AddServer | Self::Inbox | Self::Friends => false,
                }
            }
        }
    };
}
menu_screens! {
    Home, Play, Social, Servers, Profile, DressingRoom, Settings, AddServer,
    Pause, Death,
    /// OreUI-only screen.
    Inbox,
    /// OreUI-only screen.
    Friends,
    /// The invite screen over Pause while the world is hosted for friends.
    Invite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuServerTab {
    Featured,
    Favorites,
    Recent,
    Saved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuDialog {
    MinecraftImport,
    Accounts,
    Exit,
    DeathQuit,
    RemoveSaved(usize),
    RemoveShaderPack(u16, u64),
    StorageDelete,
    StorageError,
    /// A dismissible host notice whose body is supplied by the menu view.
    Notice,
    SettingsSupport(settings_support::SupportDialog),
    SettingsResetBindings(bool),
    SettingsResetGroup(settings_options::SettingsGroup),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuField {
    /// Filters the friends drawer by gamertag.
    FriendQuery,
    /// Searches options across settings categories.
    SettingsQuery,
    Name,
    Address,
    Port,
    /// The local-world create or edit screen's name field.
    WorldName,
    WorldSeed,
    WorldOption(crate::local_worlds::options::WorldOption),
    SkinName,
    RealmCode,
    /// One command macro slot's command box.
    MacroCommand(usize),
}

/// A renderer or shader-pack choice; packs carry their list revision so stale picks miss.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RendererChoice {
    /// The vanilla renderer ("Off").
    Vanilla,
    Enhanced,
    ShaderPack(u16, u64),
}

/// Joining the featured-list entry at `index`.
pub fn play_featured(view: &MenuView, index: usize) -> Option<MenuAction> {
    (index < view.featured.len()).then_some(MenuAction::PlayFeatured(index))
}

/// The vanilla screen family used by both menu history and scene composition.
pub fn menu_reference(screen: MenuScreen) -> Option<&'static str> {
    Some(match screen {
        MenuScreen::Death => Reference::DeathDeathScreen.as_str(),
        MenuScreen::Pause => Reference::PausePauseScreen.as_str(),
        MenuScreen::Home => Reference::StartStartScreen.as_str(),
        MenuScreen::Play | MenuScreen::Social | MenuScreen::Servers => {
            Reference::PlayPlayScreen.as_str()
        }
        MenuScreen::AddServer => Reference::AddExternalServerAddExternalServerScreenNew.as_str(),
        MenuScreen::Settings => Reference::SettingsScreenControlsAndSettings.as_str(),
        MenuScreen::Invite => Reference::InviteInviteScreen.as_str(),
        MenuScreen::Profile
        | MenuScreen::DressingRoom
        | MenuScreen::Inbox
        | MenuScreen::Friends => return None,
    })
}

/// The default Bedrock server port displayed by launcher address fields.
pub const DEFAULT_PORT: &str = bridge::contract::DEFAULT_BEDROCK_PORT_TEXT;

/// Host and port of a saved `host:port`; a bare host gets the default port.
pub fn split_address(address: &str) -> (String, String) {
    let literal = address
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(address);
    if literal.parse::<std::net::Ipv6Addr>().is_ok() {
        return (literal.to_owned(), DEFAULT_PORT.to_owned());
    }
    match address.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            (host.trim_matches(['[', ']']).to_owned(), port.to_owned())
        }
        _ => (address.to_owned(), DEFAULT_PORT.to_owned()),
    }
}

#[cfg(test)]
mod address_tests;

/// Maximum saved server name size in UTF-8 bytes.
pub const MAX_SERVER_NAME_BYTES: usize = 64;
/// Maximum saved server address size in UTF-8 bytes.
pub const MAX_SERVER_ADDRESS_BYTES: usize = 128;
