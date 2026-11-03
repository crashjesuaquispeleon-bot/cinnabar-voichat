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
}
