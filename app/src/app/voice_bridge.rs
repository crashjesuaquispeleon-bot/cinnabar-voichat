//! Native Rust-to-Go voice IPC lifecycle.
//!
//! The Go core owns the WebRTC peer and obtains admission from its trusted
//! broker adapter. This module forwards locally gated PCM and remote frames;
//! it never creates or trusts player identities or room identifiers.

use bevy::prelude::{Res, ResMut, Resource};
use bytes::Bytes;
use futures::StreamExt;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::runtime::network::resource_packs::PackApplication;
use client_audio::audio::{
    voice_controller::VoiceCaptureController,
    voice_input::{MicrophoneMode, VoiceInputPolicy},
    voice_remote::VoiceRemotePlayback,
    voice_wire::Pcm16VoiceFrameEncoder,
};
use client_runtime::network::NetworkHandle;
use client_shell::ui_resources::UiPresentationResource;

const BRIDGE_RETRY_DELAY: Duration = Duration::from_secs(2);
const AUDIO_TICK: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BridgeDesired {
    active: bool,
    transmit: bool,
    receive: bool,
    /// Local lifecycle discriminator only; never sent as identity to the broker or SFU.
    session: Option<(u64, u64)>,
}

impl BridgeDesired {
    const STOPPED: Self = Self {
        active: false,
        transmit: false,
        receive: false,
        session: None,
    };
}

/// Owns one optional local IPC worker for the currently joined game core.
#[derive(Resource, Default)]
pub(crate) struct VoiceBridgeRuntime {
    active: Option<ActiveBridge>,
    retry_at: Option<Instant>,
}

struct ActiveBridge {
    socket_dir: PathBuf,
    desired: tokio::sync::watch::Sender<BridgeDesired>,
    authorized: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ActiveBridge {
    fn drop(&mut self) {
        self.desired.send_replace(BridgeDesired::STOPPED);
        self.authorized.store(false, Ordering::Release);
        // Do not join on Bevy’s frame-critical thread; the watch update wakes the worker.
    }
}

impl VoiceBridgeRuntime {
    fn stop(&mut self) {
        self.active.take();
        self.retry_at = None;
    }
}

/// Room playback does not depend on local microphone consent or transmission state.
fn receive_audio_enabled(policy: &VoiceInputPolicy) -> bool {
    policy.session_active()
}

/// Probes the trusted room while joined, but opens no microphone until the user presses PTT.
/// The mobile control is exposed only after the API authorizes the two-player Cinnabar room.
/// Bevy systems receive each ECS resource as a separate parameter, so this system intentionally
/// exceeds Clippy's generic argument-count threshold.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_voice_bridge(
    network: Option<Res<NetworkHandle<PackApplication>>>,
    mut policy: Option<ResMut<VoiceInputPolicy>>,
    world: Res<crate::runtime::world::ClientWorld>,
    clock: Res<client_presentation::environment::WorldClock>,
    capture: Option<Res<VoiceCaptureController>>,
    playback: Option<Res<VoiceRemotePlayback>>,
    mut runtime: ResMut<VoiceBridgeRuntime>,
    mut presentation: Option<ResMut<UiPresentationResource>>,
) {
    let Some(policy) = policy.as_deref_mut() else {
        runtime.stop();
        if let Some(playback) = playback.as_deref() {
            playback.set_voice_session_enabled(false);
        }
        if let Some(presentation) = presentation.as_deref_mut() {
            presentation.set_voice_ptt_available(false);
        }
        return;
    };
    let session_identity = world.stream.as_ref().map(|stream| {
        (
            clock.session_generation(),
            stream.authority().actor_session_id(),
        )
    });
    let socket_dir = network
        .as_deref()
        .and_then(|network| network.core_socket_dir())
        .map(PathBuf::from);
    let (Some(session), Some(socket_dir)) = (session_identity, socket_dir) else {
        policy.set_session_active(false);
        policy.set_enabled(false);
        policy.set_mode(MicrophoneMode::Unselected);
        runtime.stop();
        if let Some(playback) = playback.as_deref() {
            playback.set_voice_session_enabled(false);
        }
        if let Some(presentation) = presentation.as_deref_mut() {
            presentation.set_voice_ptt_available(false);
        }
        return;
    };

    // This is a control-plane probe only. The policy stays disabled until a user
    // presses the already-authorized PTT button, so unsupported Bedrock worlds
    // never cause the microphone to open.
    policy.set_session_active(true);
    let desired = BridgeDesired {
        active: true,
        transmit: policy.transmission_permitted(),
        // Receiving an authorized room is independent of local microphone consent.
        // PTT and mute only gate outbound PCM; they must never prevent hearing the peer.
        receive: receive_audio_enabled(policy),
        session: Some(session),
    };
    let finished = runtime
        .active
        .as_ref()
        .is_some_and(|active| active.thread.as_ref().is_some_and(JoinHandle::is_finished));
    let changed_binding = runtime.active.as_ref().is_some_and(|active| {
        active.socket_dir != socket_dir || active.desired.borrow().session != desired.session
    });
    if changed_binding || finished {
        if let Some(mut active) = runtime.active.take() {
            let was_authorized = active.authorized.load(Ordering::Acquire);
            // A failed/closed bridge revokes both capture and PTT before any new room can start.
            if finished || active.desired.borrow().session != desired.session {
                policy.set_enabled(false);
                policy.set_mode(MicrophoneMode::Unselected);
                policy.set_push_to_talk_held(false);
            }
            active.desired.send_replace(BridgeDesired::STOPPED);
            if finished && let Some(thread) = active.thread.take() {
                let _ = thread.join();
            }
            drop(active);
            if was_authorized {
                bevy::log::info!("Cinnabar voice room authorization ended");
            }
        }
        runtime.retry_at = Some(Instant::now() + BRIDGE_RETRY_DELAY);
    }

    if let Some(active) = runtime.active.as_ref() {
        if *active.desired.borrow() != desired {
            active.desired.send_replace(desired);
        }
        let authorized = active.authorized.load(Ordering::Acquire);
        if let Some(playback) = playback.as_deref() {
            // The room can be heard while the local PTT microphone remains disabled.
            playback.set_voice_session_enabled(authorized);
        }
        if authorized && cfg!(target_os = "android") && policy.mode() == MicrophoneMode::Unselected
        {
            // Select PTT before showing the mobile button. This is not microphone consent:
            // VoiceInputPolicy remains disabled until the player presses the button.
            policy.set_mode(MicrophoneMode::PushToTalk);
            policy.set_push_to_talk_held(false);
        }
        if let Some(presentation) = presentation.as_deref_mut() {
            presentation.set_voice_ptt_available(authorized);
        }
        return;
    }

    if let Some(playback) = playback.as_deref() {
        playback.set_voice_session_enabled(false);
    }
    if let Some(presentation) = presentation.as_deref_mut() {
        presentation.set_voice_ptt_available(false);
    }
    let now = Instant::now();
    if runtime.retry_at.is_some_and(|retry_at| now < retry_at) {
        return;
    }
    if !bridge::voice_endpoint_path(&socket_dir).exists() {
        runtime.retry_at = Some(now + BRIDGE_RETRY_DELAY);
        return;
    }
    let (Some(capture), Some(playback)) = (capture, playback) else {
        return;
    };
    let queue = capture.queue();
    let remote = playback.clone();
    let (desired_tx, desired_rx) = tokio::sync::watch::channel(desired);
    let thread_socket_dir = socket_dir.clone();
    let authorized = Arc::new(AtomicBool::new(false));
    let worker_authorized = Arc::clone(&authorized);
    let worker = thread::Builder::new()
        .name("cinnabar-voice-ipc".to_owned())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run_voice_bridge(
                    thread_socket_dir,
                    queue,
                    remote,
                    desired_rx,
                    Arc::clone(&worker_authorized),
                )),
                Err(error) => Err(anyhow::Error::new(error)),
            };
            worker_authorized.store(false, Ordering::Release);
            if result.is_err() {
                bevy::log::debug!("voice IPC worker stopped; the next session tick may retry");
            }
        });
    match worker {
        Ok(thread) => {
            runtime.active = Some(ActiveBridge {
                socket_dir,
                desired: desired_tx,
                authorized,
                thread: Some(thread),
            });
            runtime.retry_at = None;
        }
        Err(_) => {
            runtime.retry_at = Some(now + BRIDGE_RETRY_DELAY);
            bevy::log::warn!("voice IPC worker could not be started");
        }
    }
}

async fn run_voice_bridge(
    socket_dir: PathBuf,
    transmit_queue: client_audio::audio::voice_stream::SharedPcmQueue,
    playback: VoiceRemotePlayback,
    mut desired: tokio::sync::watch::Receiver<BridgeDesired>,
    authorized: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use bridge::{
        VOICE_FRAME_BYTES, VOICE_IPC_KIND_AUDIO, VOICE_IPC_KIND_CLOSE, VOICE_IPC_KIND_REMOTE_AUDIO,
        VOICE_IPC_KIND_STATUS,
    };

    let (mut reader, writer) = bridge::connect_voice(&socket_dir).await?;
    let mut tick = tokio::time::interval(AUDIO_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut encoder = Pcm16VoiceFrameEncoder::new();
    let mut sequence = 1_u64;
    let mut connected = false;
    let mut server_transmit_allowed = false;

    loop {
        tokio::select! {
            changed = desired.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let state = *desired.borrow_and_update();
                if !state.active {
                    transmit_queue.clear();
                    let _ = writer.send(Bytes::from_static(&[VOICE_IPC_KIND_CLOSE])).await;
                    return Ok(());
                }
                if !state.transmit {
                    server_transmit_allowed = false;
                    transmit_queue.clear();
                }
                if connected {
                    send_transmit_gate(&writer, state.transmit).await?;
                }
            }
            incoming = reader.next() => {
                let Some(incoming) = incoming else {
                    return Err(anyhow::anyhow!("voice IPC closed"));
                };
                let payload = incoming?;
                let Some(kind) = payload.first().copied() else {
                    return Err(anyhow::anyhow!("empty voice IPC frame"));
                };
                match kind {
                    VOICE_IPC_KIND_REMOTE_AUDIO => {
                        if payload.len() != VoiceRemotePlayback::REMOTE_FRAME_BYTES + 1 {
                            return Err(anyhow::anyhow!("invalid remote voice IPC frame"));
                        }
                        if desired.borrow().receive {
                            let _ = playback.ingest_frame(&payload[1..]);
                        }
                    }
                    VOICE_IPC_KIND_STATUS => {
                        let status: IpcStatus = serde_json::from_slice(&payload[1..])?;
                        match status.status.as_str() {
                            "connected" => {
                                authorized.store(true, Ordering::Release);
                                connected = true;
                                server_transmit_allowed = false;
                                let state = *desired.borrow();
                                if !state.active {
                                    let _ = writer
                                        .send(Bytes::from_static(&[VOICE_IPC_KIND_CLOSE]))
                                        .await;
                                    return Ok(());
                                }
                                send_transmit_gate(&writer, state.transmit).await?;
                            }
                            "transmitting" => {
                                let state = *desired.borrow();
                                if state.active && state.transmit {
                                    // Discard pre-acknowledgement PCM before opening the sender gate.
                                    transmit_queue.clear();
                                    server_transmit_allowed = true;
                                } else {
                                    server_transmit_allowed = false;
                                    send_transmit_gate(&writer, false).await?;
                                }
                            }
                            "muted" => {
                                server_transmit_allowed = false;
                                let state = *desired.borrow();
                                if state.active && state.transmit {
                                    send_transmit_gate(&writer, true).await?;
                                }
                            }
                            "disabled" => {
                                authorized.store(false, Ordering::Release);
                                return Err(anyhow::anyhow!(
                                    "voice admission or media service unavailable"
                                ));
                            }
                            _ => return Err(anyhow::anyhow!("unknown voice IPC status")),
                        }
                    }
                    _ => return Err(anyhow::anyhow!("unknown voice IPC frame kind")),
                }
            }
            _ = tick.tick(), if connected && server_transmit_allowed => {
                // Do not build latency by draining a stale queue after a network stall.
                while transmit_queue.len() > 3 {
                    if transmit_queue.pop().is_none() {
                        break;
                    }
                }
                if let Some(frame) = transmit_queue.pop() {
                    let encoded = encoder.encode(&frame, sequence);
                    sequence = sequence.wrapping_add(1).max(1);
                    let mut payload = [0_u8; VOICE_FRAME_BYTES + 1];
                    payload[0] = VOICE_IPC_KIND_AUDIO;
                    payload[1..].copy_from_slice(&encoded);
                    writer.send(Bytes::copy_from_slice(&payload)).await?;
                }
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct IpcStatus {
    status: String,
    #[allow(dead_code)]
    code: Option<String>,
}

async fn send_transmit_gate(writer: &bridge::FrameQueue, allowed: bool) -> anyhow::Result<()> {
    use bridge::VOICE_IPC_KIND_TRANSMIT;
    writer
        .send(Bytes::from(vec![
            VOICE_IPC_KIND_TRANSMIT,
            u8::from(allowed),
        ]))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_authorized_session_can_receive_before_local_microphone_activation() {
        let mut policy = VoiceInputPolicy::default();
        policy.set_session_active(true);

        assert!(!policy.enabled());
        assert!(!policy.transmission_permitted());
        assert!(receive_audio_enabled(&policy));
    }

    #[test]
    fn a_new_local_partida_requires_a_fresh_voice_bridge() {
        let active = BridgeDesired {
            active: true,
            transmit: true,
            receive: true,
            session: Some((12, 34)),
        };
        let transfer = BridgeDesired {
            active: true,
            transmit: false,
            receive: false,
            session: Some((13, 35)),
        };
        let ptt_release = BridgeDesired {
            active: true,
            transmit: false,
            receive: true,
            session: Some((12, 34)),
        };

        assert_ne!(active.session, transfer.session);
        assert_eq!(active.session, ptt_release.session);
        assert_eq!(BridgeDesired::STOPPED.session, None);
    }
}
