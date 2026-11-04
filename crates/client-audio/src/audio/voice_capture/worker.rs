use super::{MicrophoneCatalog, MicrophoneDevice};

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

/// Commands accepted by the persistent CPAL owner worker.
///
/// The worker must execute every command on the same thread that owns the CPAL host,
/// device handles and active stream. These values contain only owned, Send-safe data;
/// native CPAL handles must never cross this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicrophoneWorkerCommand {
    /// Enumerate inputs and publish a fresh generation-scoped snapshot without
    /// interrupting an already active capture stream.
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

    /// Polls for an event without blocking the caller's frame schedule.
    pub fn try_recv(&self) -> Result<MicrophoneWorkerEvent, std::sync::mpsc::TryRecvError> {
        self.events.try_recv()
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
                    if event_tx
                        .send(MicrophoneWorkerEvent::DeviceUnavailable {
                            device_id: unavailable_id,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                match command_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(MicrophoneWorkerCommand::Refresh) => {
                        // Discovery is independent of stream lifetime. Replacing the
                        // catalog invalidates old UI tokens, but an active CPAL stream
                        // remains owned and running until the user selects another input.
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
                        let resolved_id = match &selection {
                            MicrophoneSelection::Device(id) => Some(id.clone()),
                            MicrophoneSelection::SystemDefault => catalog
                                .descriptors()
                                .into_iter()
                                .find(|device| device.is_default)
                                .map(|device| device.id),
                        };
                        // Resolve first: a stale/invalid UI token must not interrupt the
                        // currently working microphone or silently replace it.
                        match catalog.resolve_selection(&selection) {
                            Ok(device) => {
                                backend.stop();
                                active_id = None;
                                match backend.start(device) {
                                    Ok(()) if backend.is_running() => {
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
                                    Ok(()) => {
                                        // Some hosts can report an asynchronous stream error
                                        // immediately after opening. Never publish a selected
                                        // state for a stream that has already stopped.
                                        backend.stop();
                                        if event_tx
                                            .send(MicrophoneWorkerEvent::BackendError(
                                                "microphone stream stopped during startup".into(),
                                            ))
                                            .is_err()
                                        {
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
                                }
                            }
                            Err(error) => {
                                if event_tx
                                    .send(MicrophoneWorkerEvent::SelectionRejected(error))
                                    .is_err()
                                {
                                    backend.stop();
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
