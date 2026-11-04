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

mod worker;
pub use worker::{
    MicrophoneSelection, MicrophoneSelectionError, MicrophoneWorker, MicrophoneWorkerBackend,
    MicrophoneWorkerCommand, MicrophoneWorkerEvent, spawn_microphone_worker,
};

#[cfg(test)]
mod tests;
