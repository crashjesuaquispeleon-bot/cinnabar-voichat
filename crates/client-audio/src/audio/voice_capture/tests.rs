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
        Arc,
        atomic::{AtomicU32, Ordering},
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
    let MicrophoneWorkerEvent::Devices(devices) = worker
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap()
    else {
        panic!("expected refreshed device list");
    };
    assert_eq!(devices.len(), 2);
    assert_ne!(devices[0].id, devices[1].id);

    worker
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(devices[1].id.clone()),
        ))
        .unwrap();
    assert_eq!(
        worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        MicrophoneWorkerEvent::Selected {
            device_id: devices[1].id.clone()
        }
    );

    worker
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device("stale-token".to_owned()),
        ))
        .unwrap();
    assert_eq!(
        worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        MicrophoneWorkerEvent::SelectionRejected(MicrophoneSelectionError::DeviceUnavailable)
    );

    worker.shutdown();
    assert_eq!(starts.load(Ordering::Relaxed), 1);
}

#[test]
fn owner_worker_switches_exact_devices_and_stops_on_shutdown() {
    use std::rc::Rc;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
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
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(devices[0].id.clone()),
        ))
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
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(devices[1].id.clone()),
        ))
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

    // Refreshing the list must not interrupt the live stream, even though it
    // issues new generation-scoped IDs for subsequent user selections.
    let stops_before_refresh = stops.load(Ordering::Relaxed);
    worker.send(MicrophoneWorkerCommand::Refresh).unwrap();
    let MicrophoneWorkerEvent::Devices(refreshed_devices) = worker
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap()
    else {
        panic!("expected the refreshed microphone snapshot");
    };
    assert_ne!(devices[1].id, refreshed_devices[1].id);
    assert_eq!(stops.load(Ordering::Relaxed), stops_before_refresh);
    assert_eq!(selected.load(Ordering::Acquire), 2);
    assert!(running.load(Ordering::Acquire));

    // A token from the previous snapshot is rejected without stopping the active mic.
    worker
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(devices[1].id.clone()),
        ))
        .unwrap();
    assert_eq!(
        worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        MicrophoneWorkerEvent::SelectionRejected(MicrophoneSelectionError::DeviceUnavailable)
    );
    assert_eq!(selected.load(Ordering::Acquire), 2);
    assert_eq!(stops.load(Ordering::Relaxed), stops_before_refresh);
    assert!(running.load(Ordering::Acquire));

    worker.shutdown();

    assert!(!running.load(Ordering::Acquire));
    assert!(stops.load(Ordering::Relaxed) >= 3);
}

#[test]
fn owner_worker_does_not_publish_selected_when_stream_stops_during_startup() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    };

    struct ImmediateFailureBackend {
        running: Arc<AtomicBool>,
        stop_count: Arc<AtomicU32>,
    }

    impl MicrophoneWorkerBackend for ImmediateFailureBackend {
        type Device = u8;

        fn enumerate(&mut self) -> Result<Vec<(String, bool, Self::Device)>, String> {
            Ok(vec![("Built-in microphone".to_owned(), true, 1)])
        }

        fn stop(&mut self) {
            self.stop_count.fetch_add(1, Ordering::Relaxed);
            self.running.store(false, Ordering::Release);
        }

        fn start(&mut self, _device: &Self::Device) -> Result<(), String> {
            // Model a host that accepts the open request but reports an immediate
            // asynchronous stream error before the worker can publish readiness.
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::Acquire)
        }
    }

    let running = Arc::new(AtomicBool::new(false));
    let stop_count = Arc::new(AtomicU32::new(0));
    let worker_running = Arc::clone(&running);
    let worker_stops = Arc::clone(&stop_count);
    let worker = spawn_microphone_worker(move || ImmediateFailureBackend {
        running: worker_running,
        stop_count: worker_stops,
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
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(devices[0].id.clone()),
        ))
        .unwrap();
    assert_eq!(
        worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        MicrophoneWorkerEvent::BackendError(
            "microphone stream stopped during startup".to_owned()
        )
    );
    assert!(!running.load(Ordering::Acquire));
    assert_eq!(
        worker.recv_timeout(std::time::Duration::from_millis(150)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "a failed startup must not emit Selected or a phantom disconnect"
    );

    worker.send(MicrophoneWorkerCommand::Shutdown).unwrap();
    assert_eq!(
        worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        MicrophoneWorkerEvent::Stopped
    );
    worker.shutdown();
    assert!(stop_count.load(Ordering::Relaxed) >= 2);
}

#[test]
fn owner_worker_reports_disconnect_and_invalidates_the_selected_token() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(selected_id.clone()),
        ))
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
        .send(MicrophoneWorkerCommand::Select(
            MicrophoneSelection::Device(selected_id),
        ))
        .unwrap();
    assert_eq!(
        worker
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        MicrophoneWorkerEvent::SelectionRejected(MicrophoneSelectionError::DeviceUnavailable)
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
