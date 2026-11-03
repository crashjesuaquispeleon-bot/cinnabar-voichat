//! Platform-independent microphone discovery boundary.
//!
//! This module deliberately does not open devices or capture samples. The platform
//! adapter owns device handles; callers receive opaque IDs for presenting and selecting
//! devices without depending on CPAL types.

/// A locally enumerated microphone that can be presented in audio settings.
///
/// `id` is opaque and backend-owned. It is valid only for the enumeration snapshot
/// that produced it; callers must not persist it as a device identity or send it over
/// the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrophoneDevice {
    /// Opaque identifier understood only by the backend that produced this descriptor.
    pub id: String,
    /// Human-readable device name for local settings.
    pub name: String,
    /// Whether the operating system currently marks this as its default input.
    pub is_default: bool,
}

/// A generation-scoped catalog binding presentation descriptors to backend-owned handles.
///
/// The native backend should create and resolve this catalog on the same worker that
/// owns its CPAL host and devices. The generic handle is never exposed through the
/// descriptor API. Refreshing the catalog invalidates every earlier ID.
static NEXT_CATALOG_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct MicrophoneCatalog<T> {
    catalog_id: u64,
    generation: u64,
    entries: Vec<(MicrophoneDevice, T)>,
}

impl<T> Default for MicrophoneCatalog<T> {
    fn default() -> Self {
        Self {
            catalog_id: NEXT_CATALOG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            generation: 0,
            entries: Vec::new(),
        }
    }
}

impl<T> MicrophoneCatalog<T> {
    /// Replaces the current snapshot and returns descriptors for the new generation.
    /// Names are labels only; duplicate names still receive distinct IDs.
    pub fn refresh(
        &mut self,
        devices: impl IntoIterator<Item = (String, bool, T)>,
    ) -> Vec<MicrophoneDevice> {
        self.generation = self.generation.wrapping_add(1);
        self.entries = devices
            .into_iter()
            .enumerate()
            .map(|(index, (name, is_default, handle))| {
                let descriptor = MicrophoneDevice {
                    id: format!("mic-{}-{}-{index}", self.catalog_id, self.generation),
                    name,
                    is_default,
                };
                (descriptor, handle)
            })
            .collect();
        self.descriptors()
    }

    /// Invalidates every previously issued token and removes all retained handles.
    ///
    /// Call this when the backend detects device removal, host reset, or worker shutdown.
    /// A later refresh creates a new generation; stale UI selections cannot resolve.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
    }

    /// Returns current presentation-only descriptors.
    #[must_use]
    pub fn descriptors(&self) -> Vec<MicrophoneDevice> {
        self.entries
            .iter()
            .map(|(descriptor, _)| descriptor.clone())
            .collect()
    }

    /// Resolves an ID only in the current snapshot; the handle stays backend-owned.
    pub fn resolve(&self, id: &str) -> Option<&T> {
        self.entries
            .iter()
            .find(|(descriptor, _)| descriptor.id == id)
            .map(|(_, handle)| handle)
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Boundary for enumerating local microphone devices.
///
/// Implementations must perform operating-system enumeration outside Bevy's frame
/// schedule. This trait is intentionally small so tests can supply a deterministic mock
/// without opening a real microphone.
pub trait MicrophoneBackend: Send + Sync {
    /// Returns the input devices visible to the current process.
    fn enumerate_input_devices(&self) -> anyhow::Result<Vec<MicrophoneDevice>>;
}

/// Returns a default label only when it identifies exactly one enumerated device.
fn unique_default_name<'a>(names: &[String], default_name: Option<&'a str>) -> Option<&'a str> {
    let candidate = default_name?;
    let matches = names
        .iter()
        .filter(|name| name.as_str() == candidate)
        .count();
    if candidate.is_empty() || matches != 1 {
        None
    } else {
        Some(candidate)
    }
}

/// CPAL-backed local input-device discovery.
///
/// The adapter intentionally returns descriptors only. It does not retain CPAL device
/// handles, open an input stream, request permission explicitly, or capture samples.
/// IDs are indices scoped to this one enumeration result, not stable hardware IDs.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct CpalMicrophoneBackend;

#[cfg(not(target_arch = "wasm32"))]
impl MicrophoneBackend for CpalMicrophoneBackend {
    fn enumerate_input_devices(&self) -> anyhow::Result<Vec<MicrophoneDevice>> {
        use rodio::cpal::traits::{DeviceTrait, HostTrait};

        let host = rodio::cpal::default_host();
        let default_name = host
            .default_input_device()
            .and_then(|device| device.name().ok());
        let devices = host.input_devices()?;
        let names: Vec<String> = devices
            .map(|device| device.name().unwrap_or_default())
            .collect();
        let default_name = unique_default_name(&names, default_name.as_deref());

        Ok(names
            .into_iter()
            .enumerate()
            .map(|(index, name)| MicrophoneDevice {
                id: format!("cpal-snapshot-{index}"),
                // CPAL's name API cannot distinguish two devices with the same name.
                // Never mark multiple physical devices as the system default.
                is_default: default_name.as_deref() == Some(name.as_str()),
                name: if name.is_empty() {
                    format!("Input device {}", index + 1)
                } else {
                    name
                },
            })
            .collect())
    }
}

/// User's local microphone preference. Device tokens are valid only in the
/// catalog generation that issued them; `SystemDefault` is an explicit choice,
/// never an implicit fallback for a missing selected device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicrophoneSelection {
    /// Resolve the operating-system default at the time the selection is applied.
    SystemDefault,
    /// Resolve this exact opaque token in the current catalog snapshot.
    Device(String),
}

/// Commands accepted by the future CPAL owner worker.
///
/// The worker must execute every command on the same thread that owns the CPAL host,
/// device handles and active stream. These values contain only owned, Send-safe data;
/// native CPAL handles must never cross this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicrophoneWorkerCommand {
    /// Enumerate inputs and publish a fresh generation-scoped snapshot.
    Refresh,
    /// Stop the active stream and open exactly this current-snapshot selection.
    /// A stale token is an error; it must never trigger an implicit default fallback.
    Select(MicrophoneSelection),
    /// Stop capture, invalidate the catalog and terminate the owner worker.
    Shutdown,
}

/// Events returned by the CPAL owner worker to the local settings/controller layer.
///
/// Device IDs are presentation tokens scoped to one catalog generation. They are not
/// hardware identities and must not be persisted or transmitted to other players.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicrophoneWorkerEvent {
    /// A refreshed local device snapshot.
    Devices(Vec<MicrophoneDevice>),
    /// The requested device was opened successfully.
    Selected { device_id: String },
    /// The requested selection could not be resolved in the current snapshot.
    SelectionRejected(MicrophoneSelectionError),
    /// The active stream stopped or its device disappeared.
    DeviceUnavailable { device_id: Option<String> },
    /// Worker has stopped and invalidated all previously issued IDs.
    Stopped,
}

/// Why a microphone preference could not be resolved against the current snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrophoneSelectionError {
    /// The current snapshot has no device marked as the system default.
    NoDefaultDevice,
    /// The token is stale, unknown, or its device is no longer present.
    DeviceUnavailable,
    /// The backend reported more than one default; it must not guess.
    AmbiguousDefault,
}

impl<T> MicrophoneCatalog<T> {
    /// Resolves an explicit preference without silently substituting another device.
    ///
    /// The returned handle remains borrowed from this catalog and must be used only
    /// by the backend owner that owns the catalog and its native device handles.
    pub fn resolve_selection(
        &self,
        selection: &MicrophoneSelection,
    ) -> Result<&T, MicrophoneSelectionError> {
        match selection {
            MicrophoneSelection::Device(id) => self
                .resolve(id)
                .ok_or(MicrophoneSelectionError::DeviceUnavailable),
            MicrophoneSelection::SystemDefault => {
                let mut defaults = self
                    .entries
                    .iter()
                    .filter(|(descriptor, _)| descriptor.is_default);
                let Some((_, handle)) = defaults.next() else {
                    return Err(MicrophoneSelectionError::NoDefaultDevice);
                };
                if defaults.next().is_some() {
                    return Err(MicrophoneSelectionError::AmbiguousDefault);
                }
                Ok(handle)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockMicrophoneBackend {
        devices: Vec<MicrophoneDevice>,
    }

    impl MicrophoneBackend for MockMicrophoneBackend {
        fn enumerate_input_devices(&self) -> anyhow::Result<Vec<MicrophoneDevice>> {
            Ok(self.devices.clone())
        }
    }

    #[test]
    fn explicit_selection_resolves_exact_device_even_with_duplicate_names() {
        let mut catalog = MicrophoneCatalog::default();
        let devices = catalog.refresh([
            ("USB Microphone".to_owned(), true, 10_u32),
            ("USB Microphone".to_owned(), false, 20_u32),
        ]);

        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::Device(devices[1].id.clone())),
            Ok(&20)
        );
    }

    #[test]
    fn explicit_missing_device_never_falls_back_to_system_default() {
        let mut catalog = MicrophoneCatalog::default();
        let old = catalog.refresh([("Old microphone".to_owned(), true, 10_u32)]);
        catalog.refresh([("Replacement microphone".to_owned(), true, 20_u32)]);

        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::Device(old[0].id.clone())),
            Err(MicrophoneSelectionError::DeviceUnavailable)
        );
    }

    #[test]
    fn invalidating_catalog_rejects_old_tokens_and_removes_handles() {
        let mut catalog = MicrophoneCatalog::default();
        let devices = catalog.refresh([("USB Microphone".to_owned(), true, 10_u32)]);
        let generation = catalog.generation();

        catalog.invalidate();

        assert!(catalog.generation() > generation);
        assert!(catalog.descriptors().is_empty());
        assert_eq!(catalog.resolve(&devices[0].id), None);
        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::Device(devices[0].id.clone())),
            Err(MicrophoneSelectionError::DeviceUnavailable)
        );
        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::SystemDefault),
            Err(MicrophoneSelectionError::NoDefaultDevice)
        );
    }

    #[test]
    fn system_default_is_resolved_from_the_current_generation() {
        let mut catalog = MicrophoneCatalog::default();
        catalog.refresh([
            ("Old default".to_owned(), true, 10_u32),
            ("Other device".to_owned(), false, 20_u32),
        ]);
        catalog.refresh([
            ("Other device".to_owned(), false, 20_u32),
            ("New default".to_owned(), true, 30_u32),
        ]);

        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::SystemDefault),
            Ok(&30)
        );
    }

    #[test]
    fn system_default_rejects_missing_or_ambiguous_default() {
        let mut catalog = MicrophoneCatalog::default();
        catalog.refresh([("No default".to_owned(), false, 10_u32)]);
        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::SystemDefault),
            Err(MicrophoneSelectionError::NoDefaultDevice)
        );

        catalog.refresh([
            ("First default".to_owned(), true, 10_u32),
            ("Second default".to_owned(), true, 20_u32),
        ]);
        assert_eq!(
            catalog.resolve_selection(&MicrophoneSelection::SystemDefault),
            Err(MicrophoneSelectionError::AmbiguousDefault)
        );
    }

    #[test]
    fn catalog_resolves_exact_handle_when_device_names_are_duplicated() {
        let mut catalog = MicrophoneCatalog::default();
        let devices = catalog.refresh([
            ("USB Microphone".to_owned(), true, 10_u32),
            ("USB Microphone".to_owned(), false, 20_u32),
        ]);
        assert_ne!(devices[0].id, devices[1].id);
        assert_eq!(catalog.resolve(&devices[0].id), Some(&10));
        assert_eq!(catalog.resolve(&devices[1].id), Some(&20));
    }

    #[test]
    fn catalog_refresh_invalidates_ids_from_the_previous_generation() {
        let mut catalog = MicrophoneCatalog::default();
        let old = catalog.refresh([("Old device".to_owned(), true, 10_u32)]);
        let new = catalog.refresh([("New device".to_owned(), true, 20_u32)]);
        assert!(catalog.generation() > 1);
        assert!(catalog.resolve(&old[0].id).is_none());
        assert_eq!(catalog.resolve(&new[0].id), Some(&20));
    }

    #[test]
    fn catalog_ids_are_scoped_to_the_catalog_instance() {
        let mut first = MicrophoneCatalog::default();
        let mut second = MicrophoneCatalog::default();
        let first_devices = first.refresh([("Same device".to_owned(), true, 1_u32)]);
        let second_devices = second.refresh([("Same device".to_owned(), true, 2_u32)]);

        assert_ne!(first_devices[0].id, second_devices[0].id);
        assert!(first.resolve(&second_devices[0].id).is_none());
        assert!(second.resolve(&first_devices[0].id).is_none());
    }

    #[test]
    fn catalog_refresh_can_represent_device_disappearance() {
        let mut catalog = MicrophoneCatalog::default();
        let old = catalog.refresh([("USB Microphone".to_owned(), true, 10_u32)]);
        assert_eq!(catalog.resolve(&old[0].id), Some(&10));
        assert!(catalog.refresh(std::iter::empty()).is_empty());
        assert!(catalog.resolve(&old[0].id).is_none());
    }

    #[test]
    fn mock_backend_exposes_only_local_device_descriptors() {
        let backend = MockMicrophoneBackend {
            devices: vec![MicrophoneDevice {
                id: "mock-device-1".to_owned(),
                name: "Test microphone".to_owned(),
                is_default: true,
            }],
        };

        assert_eq!(
            backend.enumerate_input_devices().unwrap(),
            vec![MicrophoneDevice {
                id: "mock-device-1".to_owned(),
                name: "Test microphone".to_owned(),
                is_default: true,
            }]
        );
    }

    #[test]
    fn mock_backend_can_represent_no_available_microphones() {
        let backend = MockMicrophoneBackend { devices: vec![] };
        assert!(backend.enumerate_input_devices().unwrap().is_empty());
    }

    #[test]
    fn duplicate_device_names_do_not_claim_multiple_system_defaults() {
        let names = vec![
            "USB Microphone".to_owned(),
            "USB Microphone".to_owned(),
            "Built-in Microphone".to_owned(),
        ];
        let unique_default = unique_default_name(&names, Some("USB Microphone"));

        assert!(unique_default.is_none());
    }

    #[test]
    fn unique_default_name_is_preserved() {
        let names = vec![
            "USB Microphone".to_owned(),
            "Built-in Microphone".to_owned(),
        ];
        let unique_default = unique_default_name(&names, Some("USB Microphone"));

        assert_eq!(unique_default, Some("USB Microphone"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cpal_backend_is_safe_to_move_to_a_worker_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CpalMicrophoneBackend>();
    }
}
