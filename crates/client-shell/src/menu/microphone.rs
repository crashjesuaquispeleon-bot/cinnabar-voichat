impl super::MenuRuntime {
    /// Whether any device rows currently belong to the visible microphone snapshot.
    pub fn has_microphone_devices(&self) -> bool {
        !self.microphone_devices.is_empty()
    }

    /// Consumes one request to retry the operating-system microphone permission probe.
    pub fn take_microphone_permission_retry_request(&mut self) -> bool {
        std::mem::take(&mut self.microphone_permission_retry_requested)
    }

    /// Consumes one UI request to enumerate native microphone inputs.
    pub fn take_microphone_refresh_request(&mut self) -> bool {
        std::mem::take(&mut self.microphone_refresh_requested)
    }

    /// Consumes one transient device choice. The opaque token is never saved.
    pub fn take_microphone_selection_request(
        &mut self,
    ) -> Option<launcher::menu::view::MicrophoneSelectionRequest> {
        self.microphone_selection_requested.take()
    }

    /// Publishes a fresh device snapshot from the native capture worker.
    ///
    /// Refresh invalidates every previous token, so clear a prior selection before exposing
    /// the replacement list. The next choice must come from this new revision.
    pub fn publish_microphone_devices(
        &mut self,
        devices: Vec<launcher::menu::view::MicrophoneDeviceView>,
    ) {
        self.microphone_devices_revision = self.microphone_devices_revision.wrapping_add(1);
        self.microphone_devices = devices;
        self.selected_microphone_id = None;
        self.microphone_message = Some(if self.microphone_devices.is_empty() {
            "No microphone input devices were found.".to_owned()
        } else {
            format!(
                "Found {} microphone input device(s). Choose one below.",
                self.microphone_devices.len()
            )
        });
    }

    /// Marks the exact token returned by a successful worker selection event.
    pub fn mark_microphone_selected(&mut self, device_id: String) {
        if self
            .microphone_devices
            .iter()
            .any(|device| device.id == device_id)
        {
            self.selected_microphone_id = Some(device_id);
            self.microphone_message = Some("Microphone input changed successfully.".to_owned());
        } else {
            self.selected_microphone_id = None;
            self.microphone_message = Some(
                "The microphone list changed. Refresh it before selecting an input.".to_owned(),
            );
        }
    }

    /// Drops all generation-scoped rows after a device disconnect or worker shutdown.
    pub fn clear_microphone_devices(&mut self, message: impl Into<String>) {
        self.microphone_devices_revision = self.microphone_devices_revision.wrapping_add(1);
        self.microphone_devices.clear();
        self.selected_microphone_id = None;
        self.microphone_message = Some(message.into());
    }

    /// Publishes a local-only status or error from the native microphone worker.
    pub fn set_microphone_message(&mut self, message: impl Into<String>) {
        self.microphone_message = Some(message.into());
    }

    /// Publishes the capture policy's best-effort local OS permission state.
    pub fn set_microphone_permission_status(
        &mut self,
        status: launcher::menu::view::MicrophonePermissionStatus,
    ) {
        self.microphone_permission_status = status;
    }
}
