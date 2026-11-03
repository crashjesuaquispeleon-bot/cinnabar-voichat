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
    /// A backend operation failed without substituting another device.
    BackendError(String),
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

/// Operations that must run on the microphone worker's owner thread.
///
/// This trait intentionally has no `Send` or `Sync` supertrait: implementations may
/// contain CPAL host/device/stream values that must never leave the thread that created
/// them. Construct the backend inside the factory passed to
/// [`spawn_microphone_worker`], not on the caller's thread.
pub trait MicrophoneWorkerBackend: 'static {
    /// Native device handle; it remains private to the worker thread.
    type Device: 'static;

    /// Enumerate current inputs, returning (display name, is system default, handle).
    fn enumerate(&mut self) -> Result<Vec<(String, bool, Self::Device)>, String>;

    /// Stop and release any active input stream.
    fn stop(&mut self);

    /// Open exactly the supplied handle. Must not silently select another device.
    fn start(&mut self, device: &Self::Device) -> Result<(), String>;

    /// Whether the active stream is still healthy.
    fn is_running(&self) -> bool;
}

/// Sender/receiver/join handle for the persistent microphone owner worker.
pub struct MicrophoneWorker {
    commands: std::sync::mpsc::Sender<MicrophoneWorkerCommand>,
    events: std::sync::mpsc::Receiver<MicrophoneWorkerEvent>,
    owner: Option<std::thread::JoinHandle<()>>,
}

impl MicrophoneWorker {
    /// Submit an owned command. Native device handles never cross this channel.
    pub fn send(
        &self,
        command: MicrophoneWorkerCommand,
    ) -> Result<(), std::sync::mpsc::SendError<MicrophoneWorkerCommand>> {
        self.commands.send(command)
    }

    /// Wait for the next worker event.
    pub fn recv(&self) -> Result<MicrophoneWorkerEvent, std::sync::mpsc::RecvError> {
        self.events.recv()
    }

    /// Wait for an event up to the supplied duration.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<MicrophoneWorkerEvent, std::sync::mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    /// Request orderly shutdown and join the owner thread.
    pub fn shutdown(mut self) {
        let _ = self.commands.send(MicrophoneWorkerCommand::Shutdown);
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

impl Drop for MicrophoneWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(MicrophoneWorkerCommand::Shutdown);
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

/// Starts a persistent owner thread. The backend factory is Send, but the backend it
/// creates is not required to be Send because construction and all use happen inside
/// the spawned thread. A refresh invalidates old tokens; unplug/stream failure stops
/// capture and invalidates the catalog. A missing selection never falls back to default.
pub fn spawn_microphone_worker<B, F>(factory: F) -> std::io::Result<MicrophoneWorker>
where
    B: MicrophoneWorkerBackend,
    F: FnOnce() -> B + Send + 'static,
{
    use std::sync::mpsc;
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let owner = std::thread::Builder::new()
        .name("microphone-device-owner".to_owned())
        .spawn(move || {
            let mut backend = factory();
            let mut catalog = MicrophoneCatalog::<B::Device>::default();
            let mut active_id: Option<String> = None;
            loop {
                if active_id.is_some() && !backend.is_running() {
                    let unavailable_id = active_id.take();
                    backend.stop();
                    catalog.invalidate();
                    if event_tx.send(MicrophoneWorkerEvent::DeviceUnavailable {
                        device_id: unavailable_id,
                    }).is_err() {
                        break;
                    }
                }
                match command_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(MicrophoneWorkerCommand::Refresh) => {
                        backend.stop();
                        active_id = None;
                        match backend.enumerate() {
                            Ok(devices) => {
                                let descriptors = catalog.refresh(devices);
                                if event_tx
                                    .send(MicrophoneWorkerEvent::Devices(descriptors))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(error) => {
                                catalog.invalidate();
                                if event_tx
                                    .send(MicrophoneWorkerEvent::BackendError(error))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(MicrophoneWorkerCommand::Select(selection)) => {
                        backend.stop();
                        active_id = None;
                        let resolved_id = match &selection {
                            MicrophoneSelection::Device(id) => Some(id.clone()),
                            MicrophoneSelection::SystemDefault => catalog
                                .descriptors()
                                .into_iter()
                                .find(|device| device.is_default)
                                .map(|device| device.id),
                        };
                        let result = catalog.resolve_selection(&selection);
                        match result {
                            Ok(device) => match backend.start(device) {
                                Ok(()) => {
                                    active_id = resolved_id.clone();
                                    if event_tx
                                        .send(MicrophoneWorkerEvent::Selected {
                                            device_id: resolved_id.unwrap_or_default(),
                                        })
                                        .is_err()
                                    {
                                        backend.stop();
                                        break;
                                    }
                                }
                                Err(error) => {
                                    backend.stop();
                                    if event_tx
                                        .send(MicrophoneWorkerEvent::BackendError(error))
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            },
                            Err(error) => {
                                if event_tx
                                    .send(MicrophoneWorkerEvent::SelectionRejected(error))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(MicrophoneWorkerCommand::Shutdown) => {
                        backend.stop();
                        catalog.invalidate();
                        let _ = event_tx.send(MicrophoneWorkerEvent::Stopped);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        backend.stop();
                        catalog.invalidate();
                        break;
                    }
                }
            }
        })?;
    Ok(MicrophoneWorker {
        commands: command_tx,
        events: event_rx,
        owner: Some(owner),
    })
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
    fn owner_worker_refreshes_and_selects_exact_current_device() {
        use std::rc::Rc;
        use std::sync::{
            atomic::{AtomicU32, Ordering},
            Arc,
        };

        struct LocalOnlyBackend {
            starts: Arc<AtomicU32>,
            // Deliberately !Send: this value is constructed and retained only on the worker.
            _thread_local_marker: Rc<()>,
            running: bool,
        }

        impl MicrophoneWorkerBackend for LocalOnlyBackend {
            type Device = u32;

            fn enumerate(&mut self) -> Result<Vec<(String, bool, Self::Device)>, String> {
                Ok(vec![
                    ("USB Mic".to_owned(), true, 11),
                    ("USB Mic".to_owned(), false, 22),
                ])
            }

            fn stop(&mut self) {
                self.running = false;
            }

            fn start(&mut self, _device: &Self::Device) -> Result<(), String> {
                self.starts.fetch_add(1, Ordering::Relaxed);
                self.running = true;
                Ok(())
            }

            fn is_running(&self) -> bool {
                self.running
            }
        }

        let starts = Arc::new(AtomicU32::new(0));
        let worker_starts = Arc::clone(&starts);
        let worker = spawn_microphone_worker(move || LocalOnlyBackend {
            starts: worker_starts,
            _thread_local_marker: Rc::new(()),
            running: false,
        })
        .unwrap();

        worker.send(MicrophoneWorkerCommand::Refresh).unwrap();
        let MicrophoneWorkerEvent::Devices(devices) = worker.recv_timeout(std::time::Duration::from_secs(2)).unwrap() else {
            panic!("expected refreshed device list");
        };
        assert_eq!(devices.len(), 2);
        assert_ne!(devices[0].id, devices[1].id);

        worker.send(MicrophoneWorkerCommand::Select(MicrophoneSelection::Device(
            devices[1].id.clone(),
        ))).unwrap();
        assert_eq!(
            worker.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            MicrophoneWorkerEvent::Selected { device_id: devices[1].id.clone() }
        );

        worker.send(MicrophoneWorkerCommand::Select(MicrophoneSelection::Device(
            "stale-token".to_owned(),
        ))).unwrap();
        assert_eq!(
            worker.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            MicrophoneWorkerEvent::SelectionRejected(MicrophoneSelectionError::DeviceUnavailable)
        );

        worker.shutdown();
        assert_eq!(starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn owner_worker_switches_exact_devices_and_stops_on_shutdown() {
        use std::rc::Rc;
        use std::sync::{
            atomic::{AtomicBool, AtomicU32, Ordering},
            Arc,
        };

        struct SwitchingBackend {
            running: Arc<AtomicBool>,
            starts: Arc<AtomicU32>,
            stops: Arc<AtomicU32>,
            selected: Arc<AtomicU32>,
            // CPAL Device/Stream implementations may be !Send; the worker owns this value.
            _thread_local_marker: Rc<()>,
        }

        impl MicrophoneWorkerBackend for SwitchingBackend {
            type Device = u32;

            fn enumerate(&mut self) -> Result<Vec<(String, bool, Self::Device)>, String> {
                Ok(vec![
                    ("Built-in microphone".to_owned(), true, 1),
                    ("USB microphone".to_owned(), false, 2),
                ])
            }

            fn stop(&mut self) {
                self.stops.fetch_add(1, Ordering::Relaxed);
                self.running.store(false, Ordering::Release);
            }

            fn start(&mut self, device: &Self::Device) -> Result<(), String> {
                self.selected.store(*device, Ordering::Release);
                self.starts.fetch_add(1, Ordering::Relaxed);
                self.running.store(true, Ordering::Release);
                Ok(())
            }

            fn is_running(&self) -> bool {
                self.running.load(Ordering::Acquire)
            }
        }

        let running = Arc::new(AtomicBool::new(false));
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let selected = Arc::new(AtomicU32::new(0));
        let worker_running = Arc::clone(&running);
        let worker_starts = Arc::clone(&starts);
        let worker_stops = Arc::clone(&stops);
        let worker_selected = Arc::clone(&selected);
        let worker = spawn_microphone_worker(move || SwitchingBackend {
            running: worker_running,
            starts: worker_starts,
            stops: worker_stops,
            selected: worker_selected,
            _thread_local_marker: Rc::new(()),
        })
        .unwrap();

        worker.send(MicrophoneWorkerCommand::Refresh).unwrap();
        let MicrophoneWorkerEvent::Devices(devices) = worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
        else {
            panic!("expected the refreshed microphone snapshot");
        };

        worker
            .send(MicrophoneWorkerCommand::Select(MicrophoneSelection::Device(
                devices[0].id.clone(),
            )))
            .unwrap();
        assert_eq!(
            worker
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            MicrophoneWorkerEvent::Selected {
                device_id: devices[0].id.clone()
            }
        );
        assert_eq!(selected.load(Ordering::Acquire), 1);

        worker
            .send(MicrophoneWorkerCommand::Select(MicrophoneSelection::Device(
                devices[1].id.clone(),
            )))
            .unwrap();
        assert_eq!(
            worker
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            MicrophoneWorkerEvent::Selected {
                device_id: devices[1].id.clone()
            }
        );
        assert_eq!(selected.load(Ordering::Acquire), 2);
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        assert!(running.load(Ordering::Acquire));

        worker.shutdown();

        assert!(!running.load(Ordering::Acquire));
        assert!(stops.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn owner_worker_reports_disconnect_and_invalidates_the_selected_token() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct DisconnectBackend {
            running: Arc<AtomicBool>,
        }

        impl MicrophoneWorkerBackend for DisconnectBackend {
            type Device = u8;

            fn enumerate(&mut self) -> Result<Vec<(String, bool, Self::Device)>, String> {
                Ok(vec![("USB microphone".to_owned(), true, 7)])
            }

            fn stop(&mut self) {
                self.running.store(false, Ordering::Release);
            }

            fn start(&mut self, _device: &Self::Device) -> Result<(), String> {
                self.running.store(true, Ordering::Release);
                Ok(())
            }

            fn is_running(&self) -> bool {
                self.running.load(Ordering::Acquire)
            }
        }

        let running = Arc::new(AtomicBool::new(false));
        let worker_running = Arc::clone(&running);
        let worker = spawn_microphone_worker(move || DisconnectBackend {
            running: worker_running,
        })
        .unwrap();

        worker.send(MicrophoneWorkerCommand::Refresh).unwrap();
        let MicrophoneWorkerEvent::Devices(devices) = worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
        else {
            panic!("expected the refreshed microphone snapshot");
        };
        let selected_id = devices[0].id.clone();

        worker
            .send(MicrophoneWorkerCommand::Select(MicrophoneSelection::Device(
                selected_id.clone(),
            )))
            .unwrap();
        assert_eq!(
            worker
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            MicrophoneWorkerEvent::Selected {
                device_id: selected_id.clone()
            }
        );
        assert!(running.load(Ordering::Acquire));

        // Simulate the native backend observing an unplugged device/host failure.
        running.store(false, Ordering::Release);
        assert_eq!(
            worker
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            MicrophoneWorkerEvent::DeviceUnavailable {
                device_id: Some(selected_id.clone())
            }
        );

        // The worker invalidates the old generation and never substitutes the default.
        worker
            .send(MicrophoneWorkerCommand::Select(MicrophoneSelection::Device(
                selected_id,
            )))
            .unwrap();
        assert_eq!(
            worker
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            MicrophoneWorkerEvent::SelectionRejected(
                MicrophoneSelectionError::DeviceUnavailable
            )
        );

        worker.shutdown();
        assert!(!running.load(Ordering::Acquire));
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
