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

/// Boundary for enumerating local microphone devices.
///
/// Implementations must perform operating-system enumeration outside Bevy's frame
/// schedule. This trait is intentionally small so tests can supply a deterministic mock
/// without opening a real microphone.
pub trait MicrophoneBackend: Send + Sync {
    /// Returns the input devices visible to the current process.
    fn enumerate_input_devices(&self) -> anyhow::Result<Vec<MicrophoneDevice>>;
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

        Ok(devices
            .enumerate()
            .map(|(index, device)| {
                let name = device
                    .name()
                    .unwrap_or_else(|_| format!("Input device {}", index + 1));
                MicrophoneDevice {
                    id: format!("cpal-snapshot-{index}"),
                    is_default: default_name.as_deref() == Some(name.as_str()),
                    name,
                }
            })
            .collect())
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

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cpal_backend_is_safe_to_move_to_a_worker_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CpalMicrophoneBackend>();
    }
}
