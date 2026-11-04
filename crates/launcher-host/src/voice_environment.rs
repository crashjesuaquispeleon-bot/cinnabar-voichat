//! Pass build-time operator voice endpoint and ICE settings into the Go core child.
//!
//! These values are public routing configuration only. Never place private signing keys,
//! SFU control tokens, TURN credentials, account identifiers or room identifiers here.

use std::process::Command;

const WSS_ENDPOINT: &str = "CINNABAR_VOICE_WSS_ENDPOINT";
const ICE_SERVERS: &str = "CINNABAR_VOICE_ICE_SERVERS";

/// Forwards optional public build-time configuration to each native Go core process.
///
/// When the build variables are absent, Command inherits the launcher's runtime environment,
/// preserving local operator overrides. Empty values never erase inherited variables.
pub(crate) fn configure_voice_environment(command: &mut Command) {
    apply_voice_environment(
        command,
        option_env!("CINNABAR_VOICE_WSS_ENDPOINT"),
        option_env!("CINNABAR_VOICE_ICE_SERVERS"),
    );
}

fn apply_voice_environment(
    command: &mut Command,
    endpoint: Option<&str>,
    ice_servers: Option<&str>,
) {
    if let Some(endpoint) = non_empty(endpoint) {
        command.env(WSS_ENDPOINT, endpoint);
    }
    if let Some(ice_servers) = non_empty(ice_servers) {
        command.env(ICE_SERVERS, ice_servers);
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, process::Command};

    fn configured_value<'a>(command: &'a Command, name: &str) -> Option<&'a OsStr> {
        command
            .get_envs()
            .find_map(|(key, value)| (key == OsStr::new(name)).then_some(value).flatten())
    }

    #[test]
    fn public_operator_endpoint_and_ice_configuration_reach_the_child_process() {
        let mut command = Command::new("bedrock-core");
        apply_voice_environment(
            &mut command,
            Some(" wss://voice.example.test:8443/v1/voice "),
            Some(r#"[{"urls":["stun:stun.example.test:3478"]}]"#),
        );

        assert_eq!(
            configured_value(&command, WSS_ENDPOINT),
            Some(OsStr::new("wss://voice.example.test:8443/v1/voice"))
        );
        assert_eq!(
            configured_value(&command, ICE_SERVERS),
            Some(OsStr::new(r#"[{"urls":["stun:stun.example.test:3478"]}]"#))
        );
    }

    #[test]
    fn missing_or_empty_build_values_do_not_override_inherited_environment() {
        let mut command = Command::new("bedrock-core");
        apply_voice_environment(&mut command, None, Some("   "));
        assert_eq!(configured_value(&command, WSS_ENDPOINT), None);
        assert_eq!(configured_value(&command, ICE_SERVERS), None);
    }
}
