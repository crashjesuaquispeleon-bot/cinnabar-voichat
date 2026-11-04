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

const BRIDGE_RETRY_DELAY: Duration = Duration::from_secs(2);
const AUDIO_TICK: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BridgeDesired {
    active: bool,
    transmit: bool,
}

impl BridgeDesired {
    const STOPPED: Self = Self {
        active: false,
        transmit: false,
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
    thread: JoinHandle<()>,
}

impl Drop for ActiveBridge {
    fn drop(&mut self) {
        self.desired.send_replace(BridgeDesired::STOPPED);
        // Do not join on Bevy’s frame-critical thread; the watch update wakes the worker.
    }
}

impl VoiceBridgeRuntime {
    fn stop(&mut self) {
        self.active.take();
        self.retry_at = None;
    }
}

/// Starts the local voice pipe only while Voice Chat is enabled in a joined session.
pub(crate) fn drive_voice_bridge(
    network: Option<Res<NetworkHandle<PackApplication>>>,
    policy: Option<Res<VoiceInputPolicy>>,
    capture: Option<Res<VoiceCaptureController>>,
    playback: Option<Res<VoiceRemotePlayback>>,
    mut runtime: ResMut<VoiceBridgeRuntime>,
) {
    let Some(policy) = policy else {
        runtime.stop();
        return;
    };
    let feature_enabled =
        policy.enabled() && policy.session_active() && policy.mode() != MicrophoneMode::Unselected;
    let Some(socket_dir) = network
        .as_deref()
        .and_then(|network| network.core_socket_dir())
        .map(PathBuf::from)
        .filter(|_| feature_enabled)
    else {
        runtime.stop();
        return;
    };

    let finished = runtime
        .active
        .as_ref()
        .is_some_and(|active| active.thread.is_finished());
    let changed_session = runtime
        .active
        .as_ref()
        .is_some_and(|active| active.socket_dir != socket_dir);
    if finished || changed_session {
        if let Some(active) = runtime.active.take() {
            if finished {
                let _ = active.thread.join();
            }
        }
        runtime.retry_at = Some(Instant::now() + BRIDGE_RETRY_DELAY);
    }

    let desired = BridgeDesired {
        active: true,
        transmit: policy.transmission_permitted(),
    };
    if let Some(active) = runtime.active.as_ref() {
        if *active.desired.borrow() != desired {
            active.desired.send_replace(desired);
        }
        return;
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
    let worker = thread::Builder::new()
        .name("cinnabar-voice-ipc".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    if runtime
                        .block_on(run_voice_bridge(
                            thread_socket_dir,
                            queue,
                            remote,
                            desired_rx,
                        ))
                        .is_err()
                    {
                        bevy::log::debug!(
                            "voice IPC worker stopped; the next session tick may retry"
                        );
                    }
                }
                Err(_) => bevy::log::warn!("voice IPC runtime could not be created"),
            }
        });
    match worker {
        Ok(thread) => {
            runtime.active = Some(ActiveBridge {
                socket_dir,
                desired: desired_tx,
                thread,
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
                    server_transmit_allowed = false;
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
                        if payload.len() != VOICE_FRAME_BYTES + 1 {
                            return Err(anyhow::anyhow!("invalid remote voice IPC frame"));
                        }
                        let _ = playback.ingest_frame(&payload[1..]);
                    }
                    VOICE_IPC_KIND_STATUS => {
                        let status: IpcStatus = serde_json::from_slice(&payload[1..])?;
                        match status.status.as_str() {
                            "connected" => {
                                connected = true;
                                server_transmit_allowed = false;
                                let state = *desired.borrow();
                                if !state.active {
                                    let _ = writer.send(Bytes::from_static(&[VOICE_IPC_KIND_CLOSE])).await;
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
                                return Err(anyhow::anyhow!("voice admission or media service unavailable"));
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
