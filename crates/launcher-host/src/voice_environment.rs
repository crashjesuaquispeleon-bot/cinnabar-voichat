//! Pass build-time operator voice endpoint and public STUN settings into the Go core child.
//!
//! These are public routing values only. TURN credentials, broker signing keys, SFU control
//! tokens, account identifiers and room identifiers must never be embedded in client builds.

use std::process::Command;

const WSS_ENDPOINT: &str = "CINNABAR_VOICE_WSS_ENDPOINT";
const ICE_SERVERS: &str = "CINNABAR_VOICE_ICE_SERVERS";

/// Forwards optional public build-time configuration to each native Go core process.
///
/// When build variables are absent, Command inherits the launcher's runtime environment,
/// preserving local operator overrides. Empty or unsafe build values are ignored.
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
    if let Some(endpoint) = public_wss_endpoint(endpoint) {
        command.env(WSS_ENDPOINT, endpoint);
    }
    if let Some(ice_servers) = public_stun_configuration(ice_servers) {
        command.env(ICE_SERVERS, ice_servers);
    }
}

fn public_wss_endpoint(value: Option<&str>) -> Option<&str> {
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;
    (value.starts_with("wss://") && !value.chars().any(char::is_whitespace)).then_some(value)
}

/// Only public STUN URLs may enter a packaged client; TURN credentials are runtime secrets.
fn public_stun_configuration(value: Option<&str>) -> Option<&str> {
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;
    let servers: serde_json::Value = serde_json::from_str(value).ok()?;
    let servers = servers.as_array()?;
    if servers.is_empty() {
        return None;
    }
    for server in servers {
        let object = server.as_object()?;
        if object.contains_key("username") || object.contains_key("credential") {
            return None;
        }
        let urls = object.get("urls")?;
        let urls: Vec<&str> = if let Some(url) = urls.as_str() {
            vec![url]
        } else {
            urls.as_array()?
                .iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<Vec<_>>>()?
        };
        if urls.is_empty()
            || urls.iter().any(|url| {
                let url = url.trim().to_ascii_lowercase();
                !(url.starts_with("stun:") || url.starts_with("stuns:"))
            })
        {
            return None;
        }
    }
    Some(value)
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
    fn public_operator_endpoint_and_stun_configuration_reach_the_child() {
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

    #[test]
    fn unsafe_ice_configuration_never_enters_a_client_binary() {
        for value in [
            r#"[{"urls":["turn:turn.example.test:3478"],"username":"user","credential":"secret"}]"#,
            r#"[{"urls":["turns:turn.example.test:5349"]}]"#,
            r#"[{"urls":["https://not-stun.example.test"]}]"#,
            "not-json",
            "[]",
        ] {
            let mut command = Command::new("bedrock-core");
            apply_voice_environment(
                &mut command,
                Some("wss://voice.example.test/v1/voice"),
                Some(value),
            );
            assert_eq!(configured_value(&command, ICE_SERVERS), None, "accepted {value}");
        }
    }

    #[test]
    fn non_wss_endpoints_are_not_embedded() {
        for endpoint in ["http://voice.example.test", "ws://voice.example.test", ""] {
            let mut command = Command::new("bedrock-core");
            apply_voice_environment(&mut command, Some(endpoint), None);
            assert_eq!(configured_value(&command, WSS_ENDPOINT), None);
        }
    }
}
