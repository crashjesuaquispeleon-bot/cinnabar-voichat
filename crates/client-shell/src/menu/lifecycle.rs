use std::time::{SystemTime, UNIX_EPOCH};

impl Drop for super::MenuRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.flush_user_settings() {
            bevy::log::warn!("could not save settings at shutdown: {error}");
        }
        self.stop_sign_in();
        self.stop_catalog();
    }
}

/// Returns the current UNIX timestamp in seconds.
pub(super) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
