# Cinnabar Voice Chat

## Product contract

The first release is a Cinnabar-client-only voice feature.

- **Room mode:** global voice. Every Cinnabar client admitted to the same voice room can hear every other unmuted participant; there is no distance attenuation or proximity filter.
- **Room scope (confirmed by the user):** players can communicate only when they are in the same active Minecraft multiplayer session/partida. The voice room is bound to that exact joined session; players in different servers or different partidas cannot hear each other. "Global" means everyone in that one session can hear everyone else, regardless of in-game distance.
- **Client compatibility:** Cinnabar only. Vanilla Bedrock and Java clients are out of scope.
- **Microphone modes:** open microphone and Push to Talk (PTT), selectable in settings. The default mode and PTT key are not yet selected.
- **User controls:** local mute, microphone permission/status, input/output device selection, input sensitivity and per-user receive volume are candidate controls; their UI placement is not yet designed.

## Repository facts confirmed during initial inspection

- `crates/client-audio` owns client sound playback, audio settings and the Bevy adapter. Its current playback path uses `rodio` and decoded PCM; this is output functionality, not microphone capture.
- `crates/bridge` owns bounded framing for the local Rust/Go process stream. That stream carries Bedrock session frames and control handoff. It should not become the real-time voice media path: voice has different latency, loss, congestion and lifecycle requirements.
- `core/` is the Go integration process. The module already declares Pion WebRTC and the repository already has a browser relay contract in `crates/bridge` / `web/transport`. Their presence does **not** establish that a native voice room, SFU, signaling API or authorization path exists.
- No existing microphone-capture implementation or end-to-end voice-chat feature was identified in the inspected client audio modules. `client-audio` currently depends on `rodio` for output; there is no capture dependency in its manifest.

## Capture-backend findings (repository inspection)

- The locked dependency graph already contains **CPAL 0.15.3** through `rodio 0.20.1`; the lockfile does not show a second CPAL version. Existing native output code imports `rodio::cpal` directly in `crates/client-audio/src/named_audio/backend/native.rs` and uses its `HostTrait` / `DeviceTrait` APIs.
- This makes CPAL the lowest-friction capture candidate to prototype: it avoids introducing a second audio host stack or an immediate new direct dependency. The existing use proves the re-export is available in this crate, but it does **not** yet prove input-stream behavior, device switching, permission UX, or all-target builds.
- Existing output ownership is deliberately kept on a dedicated `audio-output` thread in `named_audio/backend/native.rs`; voice capture must have similarly explicit stream ownership and teardown, but should not reuse the game-output mixer as a microphone transport. The existing `AudioDevice::start_capture` is a pull-driven capture of *game playback* for recording/mixing, not microphone input.
- `packaging/macos/Info.plist.in` includes `NSMicrophoneUsageDescription`. Native capture now opens the default CPAL input stream when voice is enabled in an active session; the first open outcome acts as a best-effort permission probe. Native permission settings UI, denial recovery and signing/sandbox review remain outstanding.
- The crate declares `platform-specific = ["macos", "windows"]`, while its native output implementation is compiled for every non-wasm target. Confirm the actual release target matrix before deciding whether voice input is desktop-only or should include other native targets.

The native CPAL discovery adapter, bounded PCM queue, mockable stream boundary and first real CPAL capture opener now exist. The opener selects the default input device, requires an exact 48 kHz `f32` or `i16` configuration, builds and plays the input stream on a dedicated `voice-capture` owner thread, and drops the stream on that same thread during shutdown. `BoundedPcmQueue` uses the existing `crossbeam-queue` dependency's bounded `ArrayQueue`: 48 kHz mono, 20 ms / 960-sample frames, 15 slots by default (~300 ms), and non-blocking drop-newest overflow counted with atomics. The queue is safe to share across threads and its slots are allocated at construction; the callback-side push copies into a fixed-size frame and does not allocate. `voice_stream.rs` defines `VoiceCaptureOpener` / `VoiceCaptureStream`, a `MonoFrameBuilder`, mock stream tests and two caller-buffer conversion helpers (`interleaved_to_mono_f32_into` and `i16_slice_to_f32_into`) that do not allocate. The older `Vec` convenience converters may allocate and must not be used in a real-time callback. The internal queue format is 48 kHz: a CPAL opener must negotiate exactly 48 kHz for the first slice or implement and test resampling; it must never label 44.1 kHz samples as 48 kHz. The CPAL opener now performs create/play/stop/drop on its dedicated owner thread and clears queued PCM only after the stream has been dropped. Its callback downmixes interleaved `f32` or `i16` samples directly into fixed frames without a temporary vector. The opener currently supports only the OS default input device; user-selected device IDs, explicit permission UX/denial recovery, and error presentation remain pending. `VoiceCaptureController` probes/opens while voice is enabled in an active session and permission is Unknown or Granted, then stops the producer when disabled, denied or outside a session. Its bounded queue accepts samples only while `transmission_permitted()` is true; PTT release/mute/session changes advance a generation so late callback frames cannot be consumed after re-opening. The app's session-audio lifecycle adapter binds `session_active` to the presence of a `WorldStream` and resets capture when `(WorldClock::session_generation(), actor_session_id())` changes or the stream disappears. These values are local lifecycle discriminators only, never backend room authorization. Stop the producer before clearing the queue during lifecycle teardown so no callback can repopulate stale-session audio.

## Proposed high-level media flow (not implemented)

```text
Cinnabar microphone
  -> capture + permission gate
  -> bounded audio-frame queue
  -> voice codec (selection pending)
  -> authenticated real-time voice transport
  -> room media router / SFU (service ownership pending)
  -> subscribed Cinnabar clients
  -> decode + bounded jitter buffer
  -> client-audio playback mixer
```

A signaling/control plane must authenticate the Cinnabar account, bind the participant to the currently joined Minecraft session, admit it to that session's room, and revoke membership on disconnect or session change. Media routing must not trust client-supplied player identity or room membership.

### Existing local session signals (confirmed in source)

- `client-audio::SequencedAudioEvent` carries `origin_stream_session_id`, sourced from `WorldStream::authority().actor_session_id()`. This identifies the originating local world stream; it is not a globally meaningful room ID or an authenticated account identity.
- `SessionObservation::session_generation()` is already paired with the current stream identity in the session-audio adapter. `SessionAudio` also tracks dimension changes and clears retained outcomes when its observed `(generation, dimension)` changes.
- When the world stream disappears, `drain_sequenced_audio_into_session` clears unread retained messages and ends the bound audio session. This is useful lifecycle precedent, but it currently governs game sound outcomes, not voice capture or remote media.
- `client-runtime::NetworkHandle` owns the local core endpoint only while a joined session is live and exposes shutdown/retirement paths. It is a possible place to observe lifecycle transitions, not a voice-media transport.
- These identifiers are process-local lifecycle signals. A backend must receive a separately authenticated binding from a trusted Cinnabar service; it must never accept `actor_session_id`, `session_generation`, or a client-selected room name as proof of membership.

A server-routed SFU is the leading option to evaluate for global rooms. A full-mesh peer topology scales poorly as participant count grows. Do not select or implement an SFU, codec, or transport until the existing Cinnabar service/API ownership and deployment model have been inspected and an operator-controlled endpoint is identified. The Pion dependency in `core/go.mod` is only a dependency fact, not proof that Pion is the correct or currently available service boundary.

## Architectural constraints

1. Keep capture, encoding, network I/O and decoding off the Bevy frame-critical path. Use bounded queues and explicit backpressure/drop policy; never let a slow receiver stall game packet processing or rendering.
2. Keep real-time voice media separate from Bedrock packet framing. Reuse existing identity and session lifecycle facts where possible, but introduce a dedicated media transport contract.
3. Keep device APIs and capture ownership in a low-level audio/platform owner. Keep Bevy/UI integration in the appropriate client adapter; do not put Bevy dependencies into a domain crate.
4. Do not start capture until the user has granted OS permission and explicitly enabled voice. Open-mic mode may transmit only while enabled; PTT transmits only while the configured key is held. Local mute must stop transmission immediately.
5. On disconnect, reconnect, account change or world-session change, stop capture/transmission and retire room membership, queued frames and remote playback belonging to the old session.
6. Bound frame size, queue depth, participant count, decoder work and jitter-buffer memory. Validate all signaling and media metadata; do not accept arbitrary room IDs or participant identities from an untrusted client.
7. Do not record, persist or upload raw microphone audio. Diagnostics must not include voice payloads, credentials or private room tokens.
8. Preserve Cinnabar's license and third-party notices. Any new codec or capture library requires license, platform and packaging review before adoption.

## Microphone discovery boundary

`audio::voice_capture` defines a platform-independent `MicrophoneBackend` interface and a `MicrophoneDevice` descriptor. The descriptor exposes a display name, a default-device flag and an opaque backend-local ID that is valid only for the enumeration snapshot that produced it. The interface is deliberately limited to discovery: it does not open a device, capture PCM, or own a worker. This keeps CPAL types out of the audio policy and allows deterministic mock implementations in unit tests.

A native `CpalMicrophoneBackend` now implements discovery through the already-available `rodio::cpal` re-export. It enumerates CPAL input devices and returns snapshot-scoped index IDs; it does not retain device handles, open a stream, or capture samples. The default-device flag is matched by device name because this descriptor-only boundary does not retain or compare platform handles; duplicate device names can therefore make that flag ambiguous. The caller must invoke enumeration on a worker/owner thread, never from Bevy's frame schedule. OS permission prompting/denial handling, stream opening and asynchronous request/result wiring remain unimplemented. A device ID must not be persisted as a stable hardware identity or sent to a server.

## Capture review notes (provisional)

A review posted to PR #1 under the heading "Grok review — platform / capture / PCM queue" agrees with the CPAL reuse direction and recommends a dedicated stream owner, mockable capture boundary, fixed-format PCM and a local loopback acceptance test. Treat these as review proposals pending implementation and CI, not as tested behavior.

Version-specific platform correction: the repository locks CPAL **0.15.3**. Its upstream v0.15.3 README lists Linux support through **ALSA or optional JACK**, Windows through WASAPI by default, and macOS through CoreAudio. Do not describe PipeWire or PulseAudio as native CPAL 0.15.3 backends; newer CPAL documentation lists those optional backends, but that is not evidence they exist in the locked version. Linux first-release behavior must be qualified against the actual ALSA device routing and build environment.

The macOS purpose string is a real prerequisite: Apple's `NSMicrophoneUsageDescription` property is required for apps that access the microphone. Confirm signing/sandbox entitlements in the packaging path as well before shipping capture. Do not add the key alone and claim permission handling is complete.

For the callback queue, preallocate fixed-capacity storage and fixed-size frame slots before starting the stream. Avoid a per-frame `Box<[f32]>` allocation. Prefer a bounded non-blocking producer operation with an explicit overflow counter; dropping the newest frame is the simplest safe initial policy unless the selected queue explicitly supports producer-side overwrite without blocking or races. A 10–20 ms frame and 48 kHz mono are review suggestions, not yet product constants.

References:
- CPAL 0.15.3 platform support: https://github.com/RustAudio/cpal/blob/v0.15.3/README.md
- Apple microphone purpose-string requirement: https://developer.apple.com/documentation/BundleResources/Information-Property-List/NSMicrophoneUsageDescription

## Work plan and acceptance gates

### Stage 0 — Architecture and service discovery (current)

- [x] Confirm product choices: global voice within the same active Minecraft session, Cinnabar clients only, open mic + PTT.
- [x] Inspect current client playback, bridge framing and browser relay ownership at a high level.
- [x] Confirm local world-stream/session lifecycle signals and distinguish them from backend-authenticated room identity.
- [ ] Identify the authoritative Cinnabar account/session identity and the service that can authorize a room for the same joined Minecraft server.
- [ ] Determine whether a maintained voice SFU/service already exists in the wider Cinnabar deployment; if not, document the required service and operational owner.
- [ ] Compare capture/codec/transport options against supported desktop platforms, latency, licensing and packaging.
- [ ] Define a versioned, authenticated signaling contract and bounded media-frame contract before implementing network transport.

### Stage 1 — Local audio vertical slice

- [x] Add a device-independent `VoiceInputPolicy` in `client-audio` and register it as a client resource. It gates capture on OS permission, explicit enablement and an active multiplayer session; it gates transmission separately for open-mic/PTT and local mute.
- [x] Add deterministic tests for permission gating, open-mic, PTT press/release, mute, disconnect, permission revocation, disable and mode changes.
- [x] Implement native CPAL input-device enumeration behind `MicrophoneBackend`; it is a synchronous discovery adapter and must be called off the Bevy frame schedule.
- [x] Implement a dedicated owner-thread CPAL opener with a synchronous ready/error handshake and exact 48 kHz `f32` / `i16` negotiation.
- [x] Rate-limit transient microphone reopen attempts with bounded exponential backoff and clear stale PCM before reopening.
- [x] Wire capture policy to the active `WorldStream` lifecycle; stop and clear on disconnect, transfer or stream-generation change.
- [ ] Add OS permission UX/denial recovery, device selection and error presentation; the opener currently uses the OS default input only.
- [x] Define mockable `VoiceCaptureOpener` / `VoiceCaptureStream` traits, fixed-frame accumulation, allocation-free conversion helpers, mock lifecycle tests, and the first native CPAL stream owner.
- [ ] Connect physical PTT input and settings to the policy.
- [x] Add a bounded, thread-safe PCM frame queue with fixed internal format and explicit overflow accounting (no live capture producer yet).
- [ ] Implement a bounded capture-to-playback loopback test and verify device shutdown on mute, disconnect and application exit.
- [ ] Add device selection and input-level configuration without changing existing game sound behavior.

### Stage 2 — Authenticated global rooms

- [ ] Implement server-side room admission bound to Cinnabar identity and the active game-session identity.
- [ ] Implement signaling, media routing, participant lifecycle and reconnect behavior.
- [ ] Add remote decode, jitter buffering, receive-volume controls and participant speaking/mute status.
- [ ] Verify two-client and multi-client sessions, session isolation and cleanup under packet loss and reconnect.

### Stage 3 — Product integration and qualification

- [ ] Add settings and in-game voice controls using the existing UI ownership rules.
- [ ] Qualify Windows, macOS and Linux capture/playback and device switching.
- [ ] Measure CPU, memory, bandwidth, end-to-end latency and queue drops at representative room sizes.
- [ ] Document server deployment, configuration, privacy behavior and troubleshooting.

## Local capture and permission probe implemented

- `VoiceInputPolicy` remains closed by default: permission unknown, voice disabled, no active multiplayer session and no activation mode selected.
- Native capture uses CPAL 0.15.3 through the existing `rodio::cpal` re-export. The default input device is opened on a dedicated owner thread; the current first slice accepts exact 48 kHz F32/I16 input only.
- The first open attempt is the Stage-1 permission probe. A successful stream open marks permission Granted; explicit permission/access-denied-like errors mark Denied; missing devices and unsupported formats remain retryable. Transient open failures use an exponential retry delay beginning at 500 ms and capped at 8 seconds, rather than retrying on every Bevy frame. A successful open or a deliberate session stop resets the delay. The controller closes and clears both PCM queues before a reopen attempt so frames from a failed/stopped stream cannot leak into a later capture. Error-string classification is best-effort, not a portable OS permission API or native permission dialog.
- `VoiceCaptureController` opens only while voice is enabled and a multiplayer session is active. Unknown permission is probed once the controller attempts to open; Denied does not spin-reopen.
- The capture callback writes fixed 20 ms mono frames into a bounded, preallocated queue. It does not allocate, log, encode or block.
- The queue has a policy gate and generation tag. It accepts samples only while `transmission_permitted()` is true; PTT keeps the device open but does not retain microphone PCM until the held action is active. Mute, release, mode change, disconnect and session replacement close the gate, advance its epoch and retire stale in-flight frames.
- macOS packaging includes `NSMicrophoneUsageDescription`. Native settings/deep-link UX, denial recovery, device selection, runtime loopback validation and network transport are still pending. A pull-driven loopback source and optional existing-mixer attachment are implemented in source but are not build- or device-verified.

The permission probe may open the microphone device while voice is enabled in a live session even if PTT is not held. The queue gate prevents samples from being retained or forwarded during that interval.

## Input and lifecycle adapter findings

- Physical desktop key translation is owned by `crates/client-shell/src/physical_input.rs`; the semantic input domain is `crates/input`; the Bevy-facing collection and routing adapter is `app/src/semantic_controls.rs` (with physical sampling in `app/src/semantic_controls/physical.rs`). The audio crate should not poll Bevy keyboard state or own persisted keybinding UI.
- `semantic_input::ActionSnapshot` exposes per-action pressed/held/released phases and authority-release information. `Action::PushToTalk` is now a held semantic action, exposed as the remappable `key.pushToTalk` control with default V. The app adapter forwards the finalized held phase to `VoiceInputPolicy`; `client-audio` does not poll keyboard state.
- The Bevy app configures the session transition systems `drive_session`, `follow_server_transfer` and `recover_session_failure` before its session-audio drain. Voice lifecycle integration belongs at this app adapter boundary so it observes the same join/leave/transfer outcomes, while capture/device ownership remains in `client-audio`. The local `session_generation` is useful to invalidate local capture queues, but is not backend authorization.
- `client-audio`'s architecture policy keeps the audio owner below app/UI orchestration. Keep the CPAL stream abstraction and bounded PCM handoff there; keep physical input and session-observation wiring in the app edge. This preserves the repository's crate direction and avoids adding Bevy input dependencies to a low-level audio backend.

## Confirmed session-isolation behavior

- A voice room belongs to one exact active multiplayer session, not merely to a server address or a global Cinnabar account pool.
- All admitted Cinnabar players in that same partida can talk regardless of distance.
- A player joining another server/session must leave the previous room before joining the new one.
- Disconnecting, changing account, changing world/server session or losing authorization must stop capture/transmission and discard old-session audio.
- The backend must derive/validate the room membership from trusted session state; clients must not choose arbitrary room IDs.

## Open decisions

- Which Cinnabar-owned service/API is authoritative for account identity, current server-session identity and voice-room authorization?
- Is there an existing deployable Cinnabar SFU, or must one be designed and operated separately?
- Which desktop platforms are in the first release? The initial qualification proposal is Windows, macOS and Linux.
- Which codec and media transport meet the selected service's constraints? Opus/WebRTC is a candidate, not a decision.
- What should the default microphone mode and PTT binding be?
- What is the room participant cap and what should happen when the cap is reached?

Until the service and identity questions are answered, the media transport and room implementation remain intentionally incomplete. This document is an architecture proposal, not evidence that voice chat is already functional.


## Push-to-talk semantic input

The semantic input domain now includes `Action::PushToTalk` as a persistent held action (not a one-shot). It is exposed in the remappable controls registry as `key.pushToTalk`; its default keyboard assignment is V, and the binding is supplied through `ControlSettings` rather than polled by `client-audio`. Users can change or unbind it through the ordinary controls binding path.

The app session-audio adapter reads only the finalized `SemanticInputSnapshot` phase for `Action::PushToTalk` and forwards its `held` value to `VoiceInputPolicy::set_push_to_talk_held()`. It runs after `ClientFrameSet::SemanticFinalize` and before `sync_voice_capture_with_policy`, so the controller observes the same frame's policy. Missing or cleared snapshots are neutral/released. The audio crate never reads Bevy keyboard state.

Router and launcher tests cover PTT press/hold/release and remapping. OS permission settings UI, mute/settings UI, runtime loopback validation and network transport remain separate follow-up work.

## PTT-aware PCM privacy gate

The capture device can remain open to avoid repeatedly opening the OS microphone for every PTT press, but the PCM queue is closed unless the selected activation mode currently permits transmission. The queue advances a generation whenever this gate changes. Each callback frame is tagged with the generation it observed, and the consumer rejects frames from an older generation. This prevents a callback already in flight during PTT release, mute or session shutdown from being replayed after the gate reopens.

This is a local capture/queue privacy boundary only. The future network sender must independently enforce the same current-session and `transmission_permitted()` checks before encoding or sending any frame.


## Local loopback mixer attachment

The microphone callback now fans each accepted 20 ms PCM frame into two independently bounded queues:
- the transmit queue, reserved for the future network encoder/sender;
- the monitor queue, consumed only by local playback.

This separation is required because queue reads are destructive. A local loopback consumer must never drain the only copy of a frame before the network sender can read it. Both queues share the same transmission-policy gate and generation invalidation, so PTT release, mute, permission loss, and session teardown close and clear both destinations together.

VoiceLoopbackSource is a pull-driven rodio source attached through the existing AudioDevice::play_source path. The attachment system waits for microphone capture and the existing named-audio device to become available and adds at most one source. The source is cancelled when capture ends. It does not open another output device. If the app has no named-audio carrier, the device is disabled, or output initialization fails, attachment remains a no-op and the client continues without local monitoring.

The loopback source is still a local monitor only. It does not implement the voice network, remote playback, room membership, or server authorization. The monitor must remain independently muteable without changing capture/transmission policy.
